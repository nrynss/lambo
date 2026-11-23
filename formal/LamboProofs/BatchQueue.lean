/-
# The candle embedder's batch queue (#125)

Model of how the in-process BGE-M3 embedder coalesces concurrent `embed()`
and `embed_batch()` calls into forwards, answers each caller, and survives a
worker death, in `src/embed/candle.rs` (paths and lines as at `aeeafb68`):

* `Pending` (l. 126), `BatchQueue` (l. 137: `push`, `push_many`,
  `request_shutdown`, `wait_for_batch`, `top_up`, `take_singles`,
  `drain_all`) and `take_prioritized` (l. 254);
* `spawn_coalescer` and `supervise` (l. 1389, 1396), `coalesce_loop`
  (l. 1412, the take, the debounce and its top-up, the exit with `drain_all`);
* `serve_take` (l. 1461) with `MAX_SINGLE_STREAK`, the per-forward pull of
  new singles, the first length chunk, `still_awaited` (l. 1633) and the
  send loop;
* `length_chunks` (l. 1528) with `FORWARD_WIDTH` and `CHUNK_SLACK_BYTES`;
* `forward_isolated` (l. 1566) with the canary (`CANARY_TEXT`, l. 1640), the
  per-text reruns and `width_checked`;
* the caller side: `embed` (l. 1708) pushes one single and waits in
  `recv_bounded` (l. 1658, `EMBED_WAIT`), `embed_batch` (l. 1737) pushes its
  texts as bulk under one lock and waits to a deadline, and
  `Handle::drop` (l. 744) requests shutdown when the last handle goes.

`MAX_BATCH`, `FORWARD_WIDTH`, `CHUNK_SLACK_BYTES` and `MAX_SINGLE_STREAK`
are the fields of `Cfg`; `Cfg.rust` is `32, 8, 64, 3`. Lengths are byte
lengths (`Txt.len`), exact naturals; there is no floating point in the
queue.

## The state machine

`St` holds the queue, the shutdown flag, the worker's position (`Pc`), the
supervisor and `Shared` liveness, and the bookkeeping the theorems read:
every request queued, the callers that gave up, what each caller observed
(`obs`), the requests whose `Pending` is gone, and every forward run. `step`
takes one event: a caller's push (`enqueue`, `enqueueMany`), a caller's
deadline (`giveUp`, the receiver dropped), the last handle's drop
(`shutdown`), one step of the worker (`work`, deterministic), the worker's
death (`die`, a panic outside `catch_unwind`), the supervisor's reaction
(`supervise`, with a spawn that may fail) and the drop of `Shared`
(`dropShared`). Any interleaving is a list of events.

Atomicity: each `work` step is one lock hold of the Rust, or one forward
with the pull before it (`serve_take` pulls under the queue lock, then
forwards without it; a push during the forward is the same as a push after
it), or one `send`. The debounce sleep is the gap between the take and the
top-up, where pushes interleave. A caller's push needs a live handle, so
`enqueue` requires no shutdown: an awaiting `embed()` borrows the embedder,
so the last handle cannot drop while a caller waits.

The forward is abstract (`Fwd`): a function of the call's index and the
texts, which may return vectors (`Out.ok`), an error or a panic. Tensor
math, tokenization, padding and messages are left out. `rowFwd` is the
shape the isolation theorems assume: each text's row computed on its own,
the call failing if any row fails, every row one width (`Uniform`).

## Results

* Exactly one answer per request. `answered_at_most_once`: in every run no
  caller observes twice (a send, a dropped sender, or its own timeout).
  `core_at_rest`, `served_when_left_alone`: once the worker has caught up,
  every request ever queued has been observed exactly once; a running
  worker (`pc ≠ down`) left alone catches up within `measure` steps
  (`work_progress`, `workN_idle`). `served_when_left_alone` is the liveness
  theorem. `eventually_all_answered` is an accounting theorem only: from
  any reachable state, shutdown, the worker's drain, the supervisor's exit
  and the drop of `Shared` leave every request observed exactly once, but
  in a wedged or dead state those observations are `.dropped` from
  `dropShared` (a permanent `Backend` error), and the recipe requests
  shutdown while callers may still be waiting, which the Rust cannot do
  (an awaiting `embed()` keeps a handle alive). The invariant
  (`Good`, `good_reachable`) keeps every request in exactly one place,
  across deaths and restarts (`restart_serves_queue`).
* No cross-wiring. `wired`: the answers being sent are, position by
  position, `forward_isolated`'s results on the texts of the requests they
  go to, after reordering by length and the per-text reruns;
  `removeDesc_eq` shows the highest-first `remove` loop takes exactly the
  chunk's requests. `answers_are_own`: with a row-wise forward and a
  canary that embeds, every answer a caller receives in any run is what its
  own text gets alone. That equality is exact in the model only: the real
  forward's rows depend on their batch up to rounding (padding, batched
  kernels), and the live test
  `embed_batch_matches_single_embeds_and_fails_empty_text_in_its_slot`
  asserts only cosine > 0.999 between a batched and a single vector. So
  on the Rust, `answers_are_own` and `forwardIsolated_rowwise` hold for the
  error class and the vector up to that tolerance, not bit for bit.
* Caps. `forwards_capped`: every forward holds at most `FORWARD_WIDTH`
  texts, each within twice any other's length plus `CHUNK_SLACK_BYTES`
  (`chunksOf_slack`, from the sorted order). `takePrioritized_length_le`,
  `topUp_cap`: a take and its top-up hold at most `MAX_BATCH`;
  `takePrioritized_perm`: nothing beyond the cap is lost.
  `lengthChunks_perm`: every text in exactly one chunk; `chunksOf_eq_lit`:
  the restated loop is the Rust's `last_mut` loop.
* Isolation (#125). `forwardIsolated_rowwise`: each caller gets
  what its text gets alone, whatever it was batched with;
  `forwardIsolated_device_down`: a failing device fails every caller
  `Unavailable` in two forwards; `forwardIsolated_width_mismatch`: one
  forward, every caller `Backend`; `forwardIsolated_length`: one answer per
  text.
* Order and fairness. `takePrioritized_fst`: a take is the queued singles
  in arrival order, then the bulk ones in arrival order; `pick_progress`:
  at most `MAX_SINGLE_STREAK` single forwards in a row while a take has
  bulk texts; `take_ends`: a take ends within `phi + 1` iterations whatever
  arrives (`|single| + 132·|bulk| + 1` with the Rust's constants,
  `phi_rust`). `take_ends` is proved about `drive`, a restatement of the
  take loop with the arrivals as input; it shares `pick` with `work`, and
  `pullOf_drive` shows `work`'s pulls obey `drive`'s discipline, but no
  theorem states the bound about runs of `step` directly.
* Shutdown. `shutdown_drains`: after shutdown a running worker
  exits only once nothing is queued or held, having answered everything;
  `pullOf_nil`: no singles are pulled once shutdown is requested.

The Rust tests modelled by `decide`: `lengthChunks_long_text_alone`,
`lengthChunks_slack_edge`, `test_healthy`, `test_one_bad`, `test_panic`,
`test_device_down`, `test_lone_bad`, `test_width_mismatch`,
`test_take_singles_first`, `test_initial_take`, `test_top_up`,
`test_recall_next_forward`, `test_stream_of_singles`,
`test_no_pull_after_shutdown`, `test_gave_up_not_forwarded`.

## What the model shows about the Rust (last section)

* `worker_death_permanent`: a worker death answers the requests in its
  hands `Backend` ("worker exited mid-batch"), permanent, though nothing was
  learned about their texts; `forward_isolated` would class that failure
  `Unavailable`. A write caught in it is refused as `LamboError::Embed` and
  its intent consumed `failed`. The path is defensive (a death "should be
  impossible").
* `lone_transient_fault_permanent`, `paired_transient_fault_recovers`,
  `test_lone_vs_pair`: a device fault that has passed by the canary is
  charged to a lone text as permanent, while two texts are rerun alone and
  embed.
* `queued_bulk_starves`, `takePrioritized_no_bulk`: the no-starvation
  guarantee is per take; with `MAX_BATCH` singles queued at every take a
  queued bulk text is never taken.
* A recall can wait for the whole rest of its take, which breaks both
  `serve_take`'s "waits for at most the one forward in flight" and
  `MAX_SINGLE_STREAK`'s "a recall waits at most one bulk forward more".
  `test_stream_of_singles` shows the mild case (the forward in flight, then
  a bulk forward). `long_recall_waits_out_the_take` shows the strong one:
  singles are length-chunked shortest first, and new singles are pulled
  before every forward while bulk texts remain, so shorter singles that
  arrive later keep overtaking a long recall. With a take of 24 bulk texts
  of 11 bytes, a 200-byte recall arriving during the first forward, and a
  5-byte single arriving every two worker steps, the recall runs at forward
  10, after all three bulk forwards. The take still ends (`take_ends`).
* `death_after_shutdown_abandons_queue`: no restart after shutdown, so the
  queue is answered only by the drop of `Shared` (harmless: no caller can
  be waiting then). `wedged_stays`, `test_spawn_failure_wedges`: after a
  failed spawn only timeouts answer.
* `forwardIsolated_empty_quirk`: `texts.len() <= 1` answers one slot for no
  texts; unreachable from `serve_take`.

## The #273 seams (Track 3)

#273 proposes `candle/queue.rs`, `candle/model.rs`, `candle/identity.rs`,
with `CandleEmbedder` and the `Embedder` impl kept in `candle/mod.rs`:

* `candle/queue.rs` (`BatchQueue`, `coalesce_loop`, `supervise`,
  `serve_take`, `length_chunks`, `forward_isolated`) carries every theorem
  here except the caller-side events: the queue passes, `length_chunks`,
  the extraction, `forward_isolated`, `pick` and `take_ends`, `work` and
  its invariants and liveness, the death and supervisor theorems.
  `Pending`, `take_prioritized`, `still_awaited`, the constants and
  `CANARY_TEXT` must move with it.
* `candle/model.rs` (`BgeM3Core`, device and loading) carries no theorem
  here. It must provide the forward `answers_are_own` and
  `forwardIsolated_rowwise` assume: rows independent of each other, one
  width, the call failing as a whole (`rowFwd`, `Uniform`).
* `candle/identity.rs` (`PinnedFile`, `LoadPlan`, `Fingerprint`,
  `weight_sha256`, the identity cache) carries none; the contract the
  identity feeds is `Contract.lean`'s.
* `candle/mod.rs` keeps `embed`, `embed_batch`, `recv_bounded` and
  `Handle::drop`, whose behaviour the events `enqueue`, `enqueueMany`,
  `giveUp`, `shutdown` and `dropShared` and the classes in `Obs.transient`
  model: the split puts the producer side of the queue in `mod.rs` and its
  consumer in `queue.rs`, and `answered_at_most_once`,
  `eventually_all_answered` and `worker_death_permanent` span both.

Not modelled: the debounce's duration and every timing bound (only order),
`EMBED_WAIT` and deadlines as times (a timeout is an event), token counts
(byte length stands in, as in the Rust), poisoned locks (`unpoison` makes a
poisoned lock behave as an unpoisoned one), the empty-text rejection
(`reject_empty`, never queued), loading (`load`, `prepare`, `LoadSlot`), the
panic message text, and memory: a death is an event, not a cause.
-/

namespace Lambo.BatchQueue

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
batch, then the canary, then each text alone). -/
def forwardIsolated (f : Fwd) (dim k : Nat) (ts : List Txt) : List Res × List (List Txt) :=
  match once f k ts with
  | some vs => (widthChecked dim vs, [ts])
  | none =>
    match once f (k + 1) [canary] with
    | none => (ts.map (fun _ => .unavailable), [ts, [canary]])
    | some _ =>
      if ts.length ≤ 1 then ([.backend], [ts, [canary]])
      else (solos f dim (k + 2) ts, [ts, [canary]] ++ ts.map (fun t => [t]))

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

/-! ## The state machine -/

/-- What a caller observes, once: the answer its oneshot carried (`res`), the
`RecvError` of a sender dropped unsent (`dropped`: `recv_bounded` and
`embed_batch` turn it into `Backend("candle coalescer dropped the request
(worker exited mid-batch)")`), or its own deadline (`timedOut`: `Unavailable`
from `recv_bounded`, `deadline_passed()` from `embed_batch`). -/
inductive Obs where
  | res (r : Res)
  | dropped
  | timedOut
  deriving DecidableEq, Repr

/-- The error classes a caller reports (`EmbedError::is_transient`):
`some true` transient, `some false` permanent, `none` a vector. -/
def Obs.transient : Obs → Option Bool
  | .res (.vec _) => none
  | .res .backend => some false
  | .res .unavailable => some true
  | .dropped => some false
  | .timedOut => some true

/-- Where the worker thread is. `down`: no worker running (before the
supervisor's first spawn, or after a death); `idle`: in `wait_for_batch`;
`debounce`: holding a take, before its top-up; `serving`: at the head of
`serve_take`'s loop; `sending`: a forward done, its answers not all sent;
`exited`: returned after `drain_all`. -/
inductive Pc where
  | down
  | idle
  | debounce (batch : List Req)
  | serving (t : Take)
  | sending (t : Take) (out : List (Req × Res))
  | exited
  deriving DecidableEq, Repr

/-- The requests the worker holds outside the queue. -/
def Pc.hand : Pc → List Req
  | .debounce b => b
  | .serving t => t.items
  | .sending t out => t.items ++ out.map Prod.fst
  | _ => []

structure St where
  queue : List Req
  /-- `BatchQueue::shutdown`. -/
  shutdown : Bool
  pc : Pc
  /-- The supervisor thread is running. -/
  sup : Bool
  /-- `Shared` is alive (some `Arc<Shared>` remains). -/
  live : Bool
  /-- Every request ever queued. -/
  issued : List Req
  /-- Requests whose caller stopped waiting (receiver dropped). -/
  gaveUp : List Req
  /-- What each caller observed, in order. -/
  obs : List (Req × Obs)
  /-- Requests whose `Pending` is gone (sent, discarded or dropped). -/
  done : List Req
  /-- Forward calls so far. -/
  calls : Nat
  /-- Every forward the device ran, in order. -/
  fwds : List (List Txt)
  deriving DecidableEq, Repr

def St.init : St := ⟨[], false, .down, true, true, [], [], [], [], 0, []⟩

/-- A request's `Pending` goes away with outcome `o`; its caller observes `o`
unless it already gave up. -/
def St.settle (s : St) (r : Req) (o : Obs) : St :=
  { s with
    done := s.done ++ [r]
    obs := if r ∈ s.gaveUp then s.obs else s.obs ++ [(r, o)] }

def settleAll (s : St) (rs : List Req) (o : Obs) : St := rs.foldl (fun s r => s.settle r o) s

/-- One step of the worker thread (`coalesce_loop` and `serve_take`), with
the forward `f` and the contract width `dim`. Deterministic; a state with
nothing to do is left as it is (blocked on the condvar, or not running). -/
def work (c : Cfg) (f : Fwd) (dim : Nat) (s : St) : St :=
  match s.pc with
  | .idle =>
    if s.queue ≠ [] then
      let r := takePrioritized s.queue c.maxBatch
      if r.1.length < c.maxBatch then { s with queue := r.2, pc := .debounce r.1 }
      else { s with queue := r.2, pc := .serving (split r.1) }
    else if s.shutdown then
      -- `wait_for_batch` returned `None`; `drain_all` fails what raced in.
      { settleAll { s with queue := [] } s.queue (.res .unavailable) with pc := .exited }
    else s
  | .debounce b =>
    let r := takePrioritized s.queue (c.maxBatch - b.length)
    { s with queue := r.2, pc := .serving (split (b ++ r.1)) }
  | .serving t =>
    let p := pullOf c s.shutdown s.queue t
    match pick c p.1 t with
    | none => { s with queue := p.2, pc := .idle }
    | some (items, t') =>
      let awaited := items.filter (fun r => r ∉ s.gaveUp)
      let s1 := settleAll { s with queue := p.2 } (items.filter (fun r => r ∈ s.gaveUp)) .dropped
      if awaited = [] then { s1 with pc := .serving t' }
      else
        let fi := forwardIsolated f dim s.calls (awaited.map (·.txt))
        { s1 with
          pc := .sending t' (awaited.zip fi.1)
          calls := s.calls + fi.2.length
          fwds := s.fwds ++ fi.2 }
  | .sending t out =>
    match out with
    | [] => { s with pc := .serving t }
    | [o] => { s.settle o.1 (.res o.2) with pc := .serving t }
    | o :: os => { s.settle o.1 (.res o.2) with pc := .sending t os }
  | _ => s

/-- The events: callers, the last handle's drop, the worker, its death, the
supervisor, and the drop of `Shared`. -/
inductive Ev where
  /-- `embed`: push one single request. -/
  | enqueue (r : Req)
  /-- `embed_batch`: `push_many` of bulk requests under one lock. -/
  | enqueueMany (rs : List Req)
  /-- A caller's deadline passes and it drops its receiver. -/
  | giveUp (r : Req)
  /-- The last adapter handle drops: `Handle::drop` → `request_shutdown`. -/
  | shutdown
  /-- One step of the worker thread. -/
  | work
  /-- The worker thread dies (a panic outside `catch_unwind`). -/
  | die
  /-- The supervisor reacts to a worker that is not running: exit on
  shutdown, otherwise spawn a new one (`spawnOk = false`: the spawn's
  `expect` panics and the supervisor dies). -/
  | supervise (spawnOk : Bool)
  /-- The last `Arc<Shared>` drops, and the queue with it. -/
  | dropShared
  deriving DecidableEq, Repr

def Pc.running : Pc → Bool
  | .idle => true
  | .debounce _ => true
  | .serving _ => true
  | .sending _ _ => true
  | _ => false

def step (c : Cfg) (f : Fwd) (dim : Nat) (s : St) : Ev → St
  | .enqueue r =>
    if s.live ∧ s.shutdown = false ∧ r.bulk = false ∧ r ∉ s.issued then
      { s with queue := s.queue ++ [r], issued := s.issued ++ [r] }
    else s
  | .enqueueMany rs =>
    if s.live ∧ s.shutdown = false ∧ rs.all (·.bulk) ∧ rs.Nodup ∧ (∀ r ∈ rs, r ∉ s.issued) then
      { s with queue := s.queue ++ rs, issued := s.issued ++ rs }
    else s
  | .giveUp r =>
    if r ∈ s.issued ∧ r ∉ s.obs.map Prod.fst then
      { s with gaveUp := s.gaveUp ++ [r], obs := s.obs ++ [(r, .timedOut)] }
    else s
  | .shutdown => { s with shutdown := true }
  | .work => work c f dim s
  | .die =>
    if s.pc.running then { settleAll s s.pc.hand .dropped with pc := .down } else s
  | .supervise ok =>
    if s.sup ∧ (s.pc = .down ∨ s.pc = .exited) then
      if s.shutdown then { s with sup := false }
      else if s.pc = .down then (if ok then { s with pc := .idle } else { s with sup := false })
      else s
    else s
  | .dropShared =>
    if s.live ∧ s.shutdown ∧ s.sup = false ∧ (s.pc = .down ∨ s.pc = .exited) then
      { settleAll { s with queue := [] } s.queue .dropped with live := false }
    else s

def run (c : Cfg) (f : Fwd) (dim : Nat) (s : St) (evs : List Ev) : St :=
  evs.foldl (step c f dim) s


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

/-- What stays queued keeps its arrival order. -/
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

theorem lengthChunks_nil (c : Cfg) : lengthChunks c [] = [] := rfl

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

theorem eraseIdx_perm {α : Type} (g : List α) (i : Nat) (h : i < g.length) :
    g.Perm (g[i] :: g.eraseIdx i) := by
  rw [List.eraseIdx_eq_take_drop_succ]
  conv => lhs; rw [← List.take_append_drop i g, List.drop_eq_getElem_cons h]
  exact List.perm_middle

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

theorem removeDesc_snd_sublist {α : Type} (g : List α) (is : List Nat) :
    (removeDesc g is).2.Sublist g := by
  induction is generalizing g with
  | nil => simp [removeDesc]
  | cons i is ih =>
    unfold removeDesc
    split
    · exact (ih _).trans (List.eraseIdx_sublist g i)
    · exact ih g

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

theorem extract_snd_sublist (g : List Req) (ch : List Nat) : (extract g ch).2.Sublist g :=
  removeDesc_snd_sublist _ _

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

theorem chunkOf_snd_sublist (c : Cfg) (g : List Req) : (chunkOf c g).2.Sublist g := by
  obtain ⟨ch, ⟨-, h, -⟩ | ⟨-, h, -⟩⟩ := chunkOf_spec c g
  · rw [h]; exact List.Sublist.refl _
  · rw [h]; exact extract_snd_sublist _ _

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
theorem forwardIsolated_length (f : Fwd) (dim k : Nat) (ts : List Txt) (h : ts ≠ []) :
    (forwardIsolated f dim k ts).1.length = ts.length := by
  unfold forwardIsolated
  split
  · next vs hv => rw [widthChecked_length, once_length hv]
  · split
    · simp
    · split
      · next hl => cases ts with
        | nil => exact absurd rfl h
        | cons t ts => simp at hl ⊢; omega
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
    simp only
    split
    · next hl =>
      cases ts with
      | nil => exact absurd rfl hne
      | cons t ts =>
        cases ts with
        | cons _ _ => simp at hl
        | nil =>
          have : row t = none := by
            cases hr : row t with
            | none => rfl
            | some _ => exact absurd (by intro x hx; simp at hx; subst hx; simp [hr]) hall
          simp [soloRes, this]
    · exact solos_rowFwd row dim (k + 2) ts

/-- Shown about the Rust: the canary judges a lone text without running it
again, so a fault of the device that has passed by the canary is charged to
a lone text as permanent (`Backend`), while the same fault on a chunk of two
lets each text run alone and both embed. A write's text is often queued
alone; `Backend` makes the write path settle it `failed` and consume its
intent (`LamboError::Embed`, see `Drain/Worker.lean`). -/
def flaky (row : Txt → Option Vec) : Fwd := fun k ts => if k = 0 then .err else rowFwd row k ts

theorem lone_transient_fault_permanent (row : Txt → Option Vec) (dim : Nat) (t : Txt)
    (hc : (row canary).isSome) :
    (forwardIsolated (flaky row) dim 0 [t]).1 = [.backend] := by
  obtain ⟨v, hv⟩ := Option.isSome_iff_exists.mp hc
  simp [forwardIsolated, once, flaky, rowFwd, hv]

theorem paired_transient_fault_recovers (row : Txt → Option Vec) (dim : Nat) (t u : Txt)
    (hc : (row canary).isSome) :
    (forwardIsolated (flaky row) dim 0 [t, u]).1 = [soloRes row dim t, soloRes row dim u] := by
  obtain ⟨v, hv⟩ := Option.isSome_iff_exists.mp hc
  have h0 : once (flaky row) 0 [t, u] = none := by simp [once, flaky]
  have h1 : once (flaky row) (0 + 1) [canary] = some [v] := by simp [once, flaky, rowFwd, hv]
  have he : solos (flaky row) dim 2 [t, u] = solos (rowFwd row) dim 2 [t, u] := by
    simp [solos, solo, once, flaky]
  unfold forwardIsolated
  rw [h0, h1]
  split
  · next hl => simp at hl
  · rw [he, solos_rowFwd]; rfl

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

/-- `a_lone_bad_text_is_permanent_when_the_canary_passes`: two forwards. -/
theorem test_lone_bad : forwardIsolated (fake 4) 4 0 [tBad] = ([.backend], [[tBad], [canary]]) := by
  decide

/-- `a_width_mismatch_fails_every_caller_without_a_retry`. -/
theorem test_width_mismatch :
    forwardIsolated (fake 4) 8 0 [tA, tBB] = ([.backend, .backend], [[tA, tBB]]) := by decide

/-- The transient asymmetry on concrete texts: a device fault on the first
forward only. -/
theorem test_lone_vs_pair :
    (forwardIsolated (fun k ts => if k = 0 then .err else fake 4 k ts) 4 0 [tA]).1 = [.backend] ∧
      (forwardIsolated (fun k ts => if k = 0 then .err else fake 4 k ts) 4 0 [tA, tBB]).1 =
        [.vec ⟨4, 1⟩, .vec ⟨4, 2⟩] := by decide

/-- The `texts.len() <= 1` guard answers one slot for no texts when the
forward fails on an empty batch. Unreachable: `serve_take` never forwards an
empty chunk (`still_awaited` empties it to a `continue`). -/
theorem forwardIsolated_empty_quirk :
    (forwardIsolated (fun _ ts => if ts = [] then .err else fake 4 0 ts) 4 0 []).1 = [.backend] := by
  decide


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
forwards run in a row while the take has bulk texts. -/
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

theorem phi_rust (t : Take) (ht : t.streak = 0) :
    phi Cfg.rust 32 t ≤ t.single.length + 132 * t.bulk.length := by
  unfold phi; split
  · omega
  · simp only [Cfg.rust, ht, Nat.sub_zero]; omega


/-! ## Invariants of the state machine -/

theorem settleAll_eq (s : St) (rs : List Req) (o : Obs) :
    settleAll s rs o =
      { s with
        done := s.done ++ rs
        obs := s.obs ++ (rs.filter (fun r => r ∉ s.gaveUp)).map (fun r => (r, o)) } := by
  induction rs generalizing s with
  | nil => simp [settleAll]
  | cons r rs ih =>
    show settleAll (s.settle r o) rs o = _
    rw [ih]
    by_cases h : r ∈ s.gaveUp
    · simp [St.settle, h]
    · simp [St.settle, h]

/-- The bookkeeping every reachable state keeps. `cons`: every request ever
queued is in exactly one place: queued, in the worker's hands, or gone.
`obsUniq`: no caller observes twice. `doneObs`: a request that is gone has
been observed (answered, dropped, or timed out before). -/
structure Core (s : St) : Prop where
  cons : ∀ r, s.issued.count r = s.queue.count r + s.pc.hand.count r + s.done.count r
  uniq : ∀ r, s.issued.count r ≤ 1
  obsUniq : ∀ r, (s.obs.map Prod.fst).count r ≤ 1
  gave : ∀ r ∈ s.gaveUp, r ∈ s.obs.map Prod.fst
  doneObs : ∀ r ∈ s.done, r ∈ s.obs.map Prod.fst
  obsWhy : ∀ r ∈ s.obs.map Prod.fst, r ∈ s.gaveUp ∨ r ∈ s.done

theorem map_fst_pair (rs : List Req) (o : Obs) : (rs.map (fun r => (r, o))).map Prod.fst = rs := by
  induction rs with
  | nil => rfl
  | cons r rs ih => simp only [List.map_cons, ih]

theorem core_transfer {s s' : St} {rs : List Req} {o : Obs} (hc : Core s)
    (hi : s'.issued = s.issued) (hg : s'.gaveUp = s.gaveUp) (hd : s'.done = s.done ++ rs)
    (ho : s'.obs = s.obs ++ (rs.filter (fun r => r ∉ s.gaveUp)).map (fun r => (r, o)))
    (hcnt : ∀ r, s'.queue.count r + s'.pc.hand.count r + rs.count r =
      s.queue.count r + s.pc.hand.count r) : Core s' := by
  have hobs : s'.obs.map Prod.fst = s.obs.map Prod.fst ++ rs.filter (fun r => r ∉ s.gaveUp) := by
    rw [ho, List.map_append, map_fst_pair]
  constructor
  · intro r; rw [hi, hd, List.count_append]; have := hc.cons r; have := hcnt r; omega
  · intro r; rw [hi]; exact hc.uniq r
  · intro r
    rw [hobs, List.count_append]
    by_cases hr : r ∈ s.gaveUp
    · have : (rs.filter (fun r => r ∉ s.gaveUp)).count r = 0 :=
        List.count_eq_zero.mpr (by simp [hr])
      have := hc.obsUniq r; omega
    · have hf : (rs.filter (fun r => r ∉ s.gaveUp)).count r = rs.count r :=
        List.count_filter (by simp [hr])
      rw [hf]
      by_cases h0 : rs.count r = 0
      · have := hc.obsUniq r; omega
      · have hc1 := hc.cons r; have hu := hc.uniq r; have hcr := hcnt r
        have hd0 : s.done.count r = 0 := by omega
        have hnd : r ∉ s.done := List.count_eq_zero.mp hd0
        have ho0 : (s.obs.map Prod.fst).count r = 0 := by
          apply List.count_eq_zero.mpr
          intro hm
          rcases hc.obsWhy r hm with h | h
          · exact hr h
          · exact hnd h
        omega
  · intro r hr; rw [hg] at hr; rw [hobs]; exact List.mem_append_left _ (hc.gave r hr)
  · intro r hr
    rw [hd] at hr; rw [hobs]
    rcases List.mem_append.mp hr with hr | hr
    · exact List.mem_append_left _ (hc.doneObs r hr)
    · by_cases hg' : r ∈ s.gaveUp
      · exact List.mem_append_left _ (hc.gave r hg')
      · exact List.mem_append_right _ (List.mem_filter.mpr ⟨hr, by simp [hg']⟩)
  · intro r hr
    rw [hobs] at hr; rw [hg, hd]
    rcases List.mem_append.mp hr with hr | hr
    · rcases hc.obsWhy r hr with h | h
      · exact Or.inl h
      · exact Or.inr (List.mem_append_left _ h)
    · exact Or.inr (List.mem_append_right _ (List.mem_filter.mp hr).1)

theorem core_keep {s s' : St} (hc : Core s) (hi : s'.issued = s.issued) (hg : s'.gaveUp = s.gaveUp)
    (hd : s'.done = s.done) (ho : s'.obs = s.obs)
    (hcnt : ∀ r, s'.queue.count r + s'.pc.hand.count r = s.queue.count r + s.pc.hand.count r) :
    Core s' :=
  core_transfer (rs := []) (o := .dropped) hc hi hg (by simp [hd]) (by simp [ho])
    (by intro r; simpa using hcnt r)

/-- Every request is answered at most once, and once nothing is queued or in
the worker's hands, every request ever queued has been answered exactly
once. -/
theorem core_at_rest {s : St} (hc : Core s) (hq : s.queue = []) (hh : s.pc.hand = []) :
    ∀ r ∈ s.issued, (s.obs.map Prod.fst).count r = 1 := by
  intro r hr
  have h1 := hc.cons r
  have hpos : 0 < s.issued.count r := List.count_pos_iff.mpr hr
  rw [hq, hh] at h1
  simp only [List.count_nil, Nat.zero_add] at h1
  have hd : r ∈ s.done := List.count_pos_iff.mp (by omega)
  have := List.count_pos_iff.mpr (hc.doneObs r hd)
  have := hc.obsUniq r
  omega


/-- `Core`, plus the shape facts the theorems below read: an exited worker
and a dropped `Shared` leave nothing queued; the caps on takes and forwards;
the forward counter; and `wired`: the answers being sent are, position by
position, `forward_isolated`'s results on the texts of the requests they go
to (no cross-wiring). -/
structure Good (c : Cfg) (f : Fwd) (dim : Nat) (s : St) : Prop extends Core s where
  exitedQ : s.pc = .exited → s.queue = [] ∧ s.shutdown = true
  deadQ : s.live = false → s.queue = [] ∧ s.shutdown = true ∧ (s.pc = .down ∨ s.pc = .exited)
  debCap : ∀ b, s.pc = .debounce b → b.length ≤ c.maxBatch
  sendCap : ∀ t out, s.pc = .sending t out → out.length ≤ c.width
  fwdCaps : ∀ b ∈ s.fwds, b.length ≤ c.width ∧ Slack c b
  calls : s.calls = s.fwds.length
  wired : ∀ t out, s.pc = .sending t out →
    ∃ pre k, (pre ++ out).map Prod.snd = (forwardIsolated f dim k ((pre ++ out).map (·.1.txt))).1

theorem good_init (c : Cfg) (f : Fwd) (dim : Nat) : Good c f dim St.init where
  cons := by simp [St.init, Pc.hand]
  uniq := by simp [St.init]
  obsUniq := by simp [St.init]
  gave := by simp [St.init]
  doneObs := by simp [St.init]
  obsWhy := by simp [St.init]
  exitedQ := by simp [St.init]
  deadQ := by simp [St.init]
  debCap := by simp [St.init]
  sendCap := by simp [St.init]
  fwdCaps := by simp [St.init]
  calls := rfl
  wired := by simp [St.init]

theorem pullOf_perm (c : Cfg) (sd : Bool) (q : List Req) (t : Take) :
    ((pullOf c sd q t).1 ++ (pullOf c sd q t).2).Perm q := by
  unfold pullOf; split
  · exact takeSingles_perm _ _
  · exact List.Perm.refl _

theorem pullOf_nil (c : Cfg) (sd : Bool) (q : List Req) (t : Take) (h : t.bulk = [] ∨ sd = true) :
    pullOf c sd q t = ([], q) := by
  unfold pullOf
  have : ¬(t.bulk ≠ [] ∧ sd = false) := by
    rintro ⟨h1, h2⟩
    rcases h with h | h
    · exact h1 h
    · rw [h] at h2; cases h2
  simp only [this, ite_false]

theorem pullOf_length (c : Cfg) (sd : Bool) (q : List Req) (t : Take) :
    (pullOf c sd q t).1.length ≤ c.maxBatch := by
  unfold pullOf; split
  · rw [takeSingles_fst]; simp only [List.length_take]; exact Nat.min_le_left _ _
  · simp

theorem split_count (b : List Req) (r : Req) : (split b).items.count r = b.count r := by
  have := perm_count (List.filter_append_perm (fun p : Req => !p.bulk) b) r
  simp only [Bool.not_not] at this
  simp only [split, Take.items]
  exact this

theorem settle_eq (s : St) (r : Req) (o : Obs) : s.settle r o = settleAll s [r] o := rfl

section work
variable {c : Cfg} {f : Fwd} {dim : Nat} {s : St}

theorem good_work_idle (hg : Good c f dim s) (hpc : s.pc = .idle) : Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  split
  · next hq =>
    have hp := takePrioritized_perm s.queue c.maxBatch
    have hl := takePrioritized_length_le s.queue c.maxBatch
    split
    · exact {
        toCore := core_keep hg.toCore rfl rfl rfl rfl (by
          intro r; have := perm_count hp r
          simp only [List.count_append, Pc.hand, hpc, List.count_nil] at this ⊢; omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by intro b hb; simp only [Pc.debounce.injEq] at hb; subst hb; exact hl
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
    · exact {
        toCore := core_keep hg.toCore rfl rfl rfl rfl (by
          intro r; have := perm_count hp r
          simp only [List.count_append, Pc.hand, hpc, split_count, List.count_nil] at this ⊢; omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
  · next hq =>
    have hq' : s.queue = [] := by simpa using hq
    split
    · next hsd =>
      rw [hq', settleAll_eq]
      exact {
        toCore := core_keep hg.toCore rfl rfl (by simp) (by simp) (by
          intro r; simp [hq', Pc.hand, hpc])
        exitedQ := fun _ => ⟨rfl, by simpa using hsd⟩
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
    · exact hg

theorem good_work_debounce (hg : Good c f dim s) (b : List Req) (hpc : s.pc = .debounce b) :
    Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  have hp := takePrioritized_perm s.queue (c.maxBatch - b.length)
  exact {
    toCore := core_keep hg.toCore rfl rfl rfl rfl (by
      intro r; have := perm_count hp r
      simp only [List.count_append, Pc.hand, hpc, split_count] at this ⊢; omega)
    exitedQ := by simp
    deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
    debCap := by simp
    sendCap := by simp
    fwdCaps := hg.fwdCaps
    calls := hg.calls
    wired := by simp }

theorem good_work_sending (hg : Good c f dim s) (t : Take) (out : List (Req × Res))
    (hpc : s.pc = .sending t out) : Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  obtain ⟨pre, k, hw⟩ := hg.wired t out hpc
  have hsc := hg.sendCap t out hpc
  split
  · exact {
      toCore := core_keep hg.toCore rfl rfl rfl rfl (by intro r; simp [Pc.hand, hpc])
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · next o =>
    rw [settle_eq, settleAll_eq]
    exact {
      toCore := core_transfer (rs := [o.1]) (o := .res o.2) hg.toCore rfl rfl rfl rfl (by
        intro r; simp only [Pc.hand, hpc, List.count_append, List.map_cons, List.count_cons,
          List.count_nil, List.map_nil]; omega)
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · next o os hos =>
    rw [settle_eq, settleAll_eq]
    exact {
      toCore := core_transfer (rs := [o.1]) (o := .res o.2) hg.toCore rfl rfl rfl rfl (by
        intro r; simp only [Pc.hand, hpc, List.count_append, List.map_cons, List.count_cons,
          List.count_nil]; omega)
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by
        intro t' out' h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
        simp at hsc; omega
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by
        intro t' out' h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
        exact ⟨pre ++ [o], k, by simpa using hw⟩ }

theorem count_filter_split (g : List Req) (items : List Req) (r : Req) :
    (items.filter (fun x => x ∉ g)).count r + (items.filter (fun x => x ∈ g)).count r =
      items.count r := by
  have h := perm_count (List.filter_append_perm (fun x : Req => decide (x ∉ g)) items) r
  have he : (fun x : Req => !decide (x ∉ g)) = (fun x => decide (x ∈ g)) := by
    funext x; simp
  rw [List.count_append, he] at h
  exact h

theorem map_txt_of_fst (out : List (Req × Res)) (aw : List Req) (h : out.map Prod.fst = aw) :
    out.map (fun x => x.1.txt) = aw.map (·.txt) := by
  rw [← h, List.map_map]; rfl

theorem good_work_serving (hw : 1 ≤ c.width) (hg : Good c f dim s) (t : Take)
    (hpc : s.pc = .serving t) : Good c f dim (work c f dim s) := by
  unfold work
  rw [hpc]
  simp only
  have hpp := pullOf_perm c s.shutdown s.queue t
  split
  · next hn =>
    obtain ⟨hb, hs⟩ := (pick_none_iff c _ t).mp hn
    have hs1 : t.single = [] := (List.append_eq_nil_iff.mp hs).1
    have hp1 : (pullOf c s.shutdown s.queue t).1 = [] := (List.append_eq_nil_iff.mp hs).2
    exact {
      toCore := core_keep hg.toCore rfl rfl rfl rfl (by
        intro r; have := perm_count hpp r
        simp only [List.count_append, hp1, List.count_nil, Pc.hand, hpc, Take.items, hb, hs1,
          List.append_nil] at this ⊢
        omega)
      exitedQ := by simp
      deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · next items t' hpick =>
    have hperm := pick_perm c _ t items t' hpick
    have hcaps := pick_items_caps c _ t items t' hw hpick
    have hsplit := count_filter_split s.gaveUp items
    split
    · next haw =>
      rw [settleAll_eq]
      exact {
        toCore := core_transfer (rs := items.filter (fun r => r ∈ s.gaveUp)) (o := .dropped)
          hg.toCore rfl rfl rfl rfl (by
            intro r
            have h1 := perm_count hperm r; have h2 := perm_count hpp r; have h3 := hsplit r
            rw [haw] at h3
            simp only [List.count_append, List.count_nil, Pc.hand, hpc] at h1 h2 h3 ⊢
            omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by simp
        fwdCaps := hg.fwdCaps
        calls := hg.calls
        wired := by simp }
    · next haw =>
      have hne : (items.filter (fun r => r ∉ s.gaveUp)).map (·.txt) ≠ [] := by simpa using haw
      have hlen := forwardIsolated_length f dim s.calls _ hne
      simp only [List.length_map] at hlen
      have hfst : ((items.filter (fun r => r ∉ s.gaveUp)).zip
          (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1).map
            Prod.fst = items.filter (fun r => r ∉ s.gaveUp) := List.map_fst_zip (by omega)
      have hsnd : ((items.filter (fun r => r ∉ s.gaveUp)).zip
          (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1).map
            Prod.snd =
          (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1 :=
        List.map_snd_zip (by omega)
      have hawl : (items.filter (fun r => r ∉ s.gaveUp)).length ≤ c.width :=
        Nat.le_trans (List.length_filter_le _ _) hcaps.1
      rw [settleAll_eq]
      exact {
        toCore := core_transfer (rs := items.filter (fun r => r ∈ s.gaveUp)) (o := .dropped)
          hg.toCore rfl rfl rfl rfl (by
            intro r
            have h1 := perm_count hperm r; have h2 := perm_count hpp r; have h3 := hsplit r
            simp only [Pc.hand, hfst, List.count_append, Pc.hand, hpc] at h1 h2 h3 ⊢
            omega)
        exitedQ := by simp
        deadQ := by intro h; have := hg.deadQ h; rw [hpc] at this; simp at this
        debCap := by simp
        sendCap := by
          intro t2 out h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
          simp only [List.length_zip]; omega
        fwdCaps := by
          intro b hb
          rcases List.mem_append.mp hb with hb | hb
          · exact hg.fwdCaps b hb
          · refine forwardIsolated_fwds_caps c f dim s.calls _ (by simpa using hawl) hw ?_ b hb
            exact hcaps.2.sublist ((List.filter_sublist).map _)
        calls := by simp [hg.calls]
        wired := by
          intro t2 out h'; simp only [Pc.sending.injEq] at h'; obtain ⟨-, rfl⟩ := h'
          refine ⟨[], s.calls, ?_⟩
          simp only [List.nil_append]
          rw [hsnd, map_txt_of_fst _ _ hfst] }

theorem good_work (hw : 1 ≤ c.width) (hg : Good c f dim s) : Good c f dim (work c f dim s) := by
  cases hpc : s.pc with
  | idle => exact good_work_idle hg hpc
  | debounce b => exact good_work_debounce hg b hpc
  | serving t => exact good_work_serving hw hg t hpc
  | sending t out => exact good_work_sending hg t out hpc
  | down => simp only [work, hpc]; exact hg
  | exited => simp only [work, hpc]; exact hg

end work


section step
variable {c : Cfg} {f : Fwd} {dim : Nat} {s : St}

theorem count_single_le (r x : Req) : [r].count x ≤ 1 := by
  rw [List.count_singleton]; split <;> omega

theorem count_single_of_ne {r x : Req} (h : x ≠ r) : [r].count x = 0 :=
  List.count_eq_zero.mpr (by simpa using h)

theorem good_enqueue (hg : Good c f dim s) (r : Req) : Good c f dim (step c f dim s (.enqueue r)) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨hl, hsd, -, hfresh⟩ := h
    exact {
      cons := by
        intro x; have := hg.cons x; simp only [List.count_append]; omega
      uniq := by
        intro x; simp only [List.count_append]
        by_cases hx : x = r
        · subst hx; rw [List.count_eq_zero.mpr hfresh]; exact Nat.le_trans (by omega) (count_single_le x x)
        · rw [count_single_of_ne hx]; exact hg.uniq x
      obsUniq := hg.obsUniq
      gave := hg.gave
      doneObs := hg.doneObs
      obsWhy := hg.obsWhy
      exitedQ := by intro he; have := (hg.exitedQ he).2; simp_all
      deadQ := by intro hd; simp_all
      debCap := hg.debCap
      sendCap := hg.sendCap
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := hg.wired }
  · exact hg

theorem good_enqueueMany (hg : Good c f dim s) (rs : List Req) :
    Good c f dim (step c f dim s (.enqueueMany rs)) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨hl, hsd, -, hnd, hfresh⟩ := h
    exact {
      cons := by
        intro x; have := hg.cons x; simp only [List.count_append]; omega
      uniq := by
        intro x; simp only [List.count_append]
        rw [List.Nodup.count hnd]
        by_cases hx : x ∈ rs
        · simp only [hx, ite_true, List.count_eq_zero.mpr (hfresh x hx)]; omega
        · simp only [hx, ite_false]; have := hg.uniq x; omega
      obsUniq := hg.obsUniq
      gave := hg.gave
      doneObs := hg.doneObs
      obsWhy := hg.obsWhy
      exitedQ := by intro he; have := (hg.exitedQ he).2; simp_all
      deadQ := by intro hd; simp_all
      debCap := hg.debCap
      sendCap := hg.sendCap
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := hg.wired }
  · exact hg

theorem good_giveUp (hg : Good c f dim s) (r : Req) : Good c f dim (step c f dim s (.giveUp r)) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨-, hno⟩ := h
    have hobs : ((s.obs ++ [(r, Obs.timedOut)]).map Prod.fst) = s.obs.map Prod.fst ++ [r] := by simp
    exact {
      cons := hg.cons
      uniq := hg.uniq
      obsUniq := by
        intro x; simp only [hobs, List.count_append]
        by_cases hx : x = r
        · subst hx; rw [List.count_eq_zero.mpr hno]; exact Nat.le_trans (by omega) (count_single_le x x)
        · rw [count_single_of_ne hx]; exact hg.obsUniq x
      gave := by
        intro x hx; simp only [hobs]
        rcases List.mem_append.mp hx with hx | hx
        · exact List.mem_append_left _ (hg.gave x hx)
        · exact List.mem_append_right _ hx
      doneObs := by intro x hx; simp only [hobs]; exact List.mem_append_left _ (hg.doneObs x hx)
      obsWhy := by
        intro x hx; simp only [hobs] at hx
        rcases List.mem_append.mp hx with hx | hx
        · rcases hg.obsWhy x hx with h | h
          · exact Or.inl (List.mem_append_left _ h)
          · exact Or.inr h
        · exact Or.inl (List.mem_append_right _ hx)
      exitedQ := hg.exitedQ
      deadQ := hg.deadQ
      debCap := hg.debCap
      sendCap := hg.sendCap
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := hg.wired }
  · exact hg

theorem good_shutdown (hg : Good c f dim s) : Good c f dim (step c f dim s .shutdown) :=
  { hg.toCore with
    exitedQ := fun h => ⟨(hg.exitedQ h).1, rfl⟩
    deadQ := fun h => ⟨(hg.deadQ h).1, rfl, (hg.deadQ h).2.2⟩
    debCap := hg.debCap
    sendCap := hg.sendCap
    fwdCaps := hg.fwdCaps
    calls := hg.calls
    wired := hg.wired }

theorem good_die (hg : Good c f dim s) : Good c f dim (step c f dim s .die) := by
  simp only [step]
  split
  · next hr =>
    rw [settleAll_eq]
    exact {
      toCore := core_transfer (rs := s.pc.hand) (o := .dropped) hg.toCore rfl rfl rfl rfl (by
        intro x; simp [Pc.hand])
      exitedQ := by simp
      deadQ := by
        intro hd; have := (hg.deadQ hd).2.2
        rcases this with h | h <;> rw [h] at hr <;> simp [Pc.running] at hr
      debCap := by simp
      sendCap := by simp
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by simp }
  · exact hg

theorem good_supervise (hg : Good c f dim s) (ok : Bool) :
    Good c f dim (step c f dim s (.supervise ok)) := by
  simp only [step]
  split
  · next h =>
    split
    · exact { hg with }
    · next hsd =>
      split
      · next hdown =>
        split
        · exact {
            toCore := core_keep hg.toCore rfl rfl rfl rfl (by intro x; simp [Pc.hand, hdown])
            exitedQ := by simp
            deadQ := by intro hd; have := (hg.deadQ hd).2.1; simp_all
            debCap := by simp
            sendCap := by simp
            fwdCaps := hg.fwdCaps
            calls := hg.calls
            wired := by simp }
        · exact { hg with }
      · exact hg
  · exact hg

theorem good_dropShared (hg : Good c f dim s) : Good c f dim (step c f dim s .dropShared) := by
  simp only [step]
  split
  · next h =>
    obtain ⟨-, hsd, -, hpc⟩ := h
    have hh : s.pc.hand = [] := by rcases hpc with h | h <;> rw [h] <;> rfl
    rw [settleAll_eq]
    exact {
      toCore := core_transfer (rs := s.queue) (o := .dropped) hg.toCore rfl rfl rfl rfl (by
        intro x; simp [hh])
      exitedQ := fun _ => ⟨rfl, hsd⟩
      deadQ := fun _ => ⟨rfl, hsd, hpc⟩
      debCap := by rcases hpc with h | h <;> simp [h]
      sendCap := by rcases hpc with h | h <;> simp [h]
      fwdCaps := hg.fwdCaps
      calls := hg.calls
      wired := by rcases hpc with h | h <;> simp [h] }
  · exact hg

/-- Every reachable state keeps the invariant. -/
theorem good_step (hw : 1 ≤ c.width) (hg : Good c f dim s) (e : Ev) : Good c f dim (step c f dim s e) := by
  cases e with
  | enqueue r => exact good_enqueue hg r
  | enqueueMany rs => exact good_enqueueMany hg rs
  | giveUp r => exact good_giveUp hg r
  | shutdown => exact good_shutdown hg
  | work => exact good_work hw hg
  | die => exact good_die hg
  | supervise ok => exact good_supervise hg ok
  | dropShared => exact good_dropShared hg

end step

theorem good_run {c : Cfg} {f : Fwd} {dim : Nat} (hw : 1 ≤ c.width) (s : St) (hg : Good c f dim s)
    (evs : List Ev) : Good c f dim (run c f dim s evs) := by
  induction evs generalizing s with
  | nil => exact hg
  | cons e es ih => exact ih _ (good_step hw hg e)

theorem good_reachable {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev) :
    Good c f dim (run c f dim St.init evs) :=
  good_run hw _ (good_init c f dim) evs


/-! ## Liveness: the worker drains -/

def Pc.rank : Pc → Nat
  | .idle => 3
  | .debounce b => if b = [] then 5 else 2
  | .serving t => if t.items = [] then 4 else 1
  | .sending _ out => if out = [] then 5 else 0
  | .down => 0
  | .exited => 0

/-- Six per request queued or held, plus where the worker is. -/
def measure (s : St) : Nat := 6 * (s.queue.length + s.pc.hand.length) + s.pc.rank

/-- The worker has something to do: it is mid-take, or idle with work queued
or shutdown requested. -/
def Busy (s : St) : Prop :=
  match s.pc with
  | .idle => s.queue ≠ [] ∨ s.shutdown = true
  | .debounce _ => True
  | .serving _ => True
  | .sending _ _ => True
  | _ => False

theorem work_not_busy {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (h : ¬Busy s) : work c f dim s = s := by
  unfold Busy at h
  unfold work
  split
  · next hpc =>
    rw [hpc] at h
    simp only [not_or, Decidable.not_not] at h
    simp [h.1, h.2]
  all_goals first | rfl | (next hpc => rw [hpc] at h; simp at h)

theorem length_count_split (b : List Req) : (split b).items.length = b.length := by
  have := (List.filter_append_perm (fun p : Req => !p.bulk) b).length_eq
  simp only [Bool.not_not] at this
  simpa [split, Take.items] using this

/-- Every step of a busy worker lowers `measure`; only a new request raises
it. With `MAX_BATCH > 0`. -/
theorem work_progress {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (hm : 0 < c.maxBatch) (h : Busy s) :
    measure (work c f dim s) < measure s := by
  unfold Busy at h
  unfold work measure
  cases hpc : s.pc with
  | down => rw [hpc] at h; exact absurd h id
  | exited => rw [hpc] at h; exact absurd h id
  | idle =>
    simp only
    split
    · next hq =>
      have hl := (takePrioritized_perm s.queue c.maxBatch).length_eq
      have hlen := takePrioritized_length s.queue c.maxBatch
      have hq0 : 0 < s.queue.length := List.length_pos_iff.mpr hq
      have hne : (takePrioritized s.queue c.maxBatch).1 ≠ [] := by
        intro h0; rw [h0] at hlen; simp at hlen; omega
      simp only [List.length_append] at hl
      split
      · simp only [Pc.hand, Pc.rank, hne, ite_false]; omega
      · have hs : (split (takePrioritized s.queue c.maxBatch).1).items ≠ [] := by
          intro h0; have := length_count_split (takePrioritized s.queue c.maxBatch).1
          rw [h0] at this; simp at this; exact hne (List.length_eq_zero_iff.mp this.symm)
        simp only [Pc.hand, Pc.rank, hs, ite_false, length_count_split]; omega
    · split
      · next hq _ =>
        have hq' : s.queue = [] := by simpa using hq
        rw [hq', settleAll_eq]; simp [Pc.hand, Pc.rank]
      · next hq hsd => rw [hpc] at h; simp at hq hsd; simp_all
  | debounce b =>
    simp only
    have hl := (takePrioritized_perm s.queue (c.maxBatch - b.length)).length_eq
    simp only [List.length_append] at hl
    have hlen := length_count_split (b ++ (takePrioritized s.queue (c.maxBatch - b.length)).1)
    simp only [List.length_append] at hlen
    by_cases hb : b = []
    · simp only [Pc.hand, Pc.rank, hb, ite_true]; split <;> simp_all <;> omega
    · have hs : (split (b ++ (takePrioritized s.queue (c.maxBatch - b.length)).1)).items ≠ [] := by
        intro h0; rw [h0] at hlen; simp at hlen; exact hb (List.length_eq_zero_iff.mp (by omega))
      simp only [Pc.hand, Pc.rank, hb, hs, ite_false, hlen]; omega
  | sending t out =>
    simp only
    split
    · simp only [Pc.hand, Pc.rank, List.map_nil, List.append_nil, ite_true]; split <;> omega
    · rw [settle_eq, settleAll_eq]; simp [Pc.hand, Pc.rank]; split <;> omega
    · next o os hos =>
      rw [settle_eq, settleAll_eq]
      simp only [Pc.hand, Pc.rank, List.map_cons, List.length_append, List.length_cons, List.length_map]
      have : os ≠ [] := hos
      simp [this]
  | serving t =>
    simp only
    have hpp := (pullOf_perm c s.shutdown s.queue t).length_eq
    simp only [List.length_append] at hpp
    split
    · next hn =>
      obtain ⟨hb, hs⟩ := (pick_none_iff c _ t).mp hn
      have hti : t.items = [] := by
        simp [Take.items, hb, (List.append_eq_nil_iff.mp hs).1]
      have hp1 : (pullOf c s.shutdown s.queue t).1 = [] := (List.append_eq_nil_iff.mp hs).2
      simp only [Pc.hand, Pc.rank, hti, ite_true, hp1, List.length_nil] at hpp ⊢
      omega
    · next items t' hpick =>
      have hperm := (pick_perm c _ t items t' hpick).length_eq
      simp only [List.length_append] at hperm
      have hne := List.length_pos_iff.mpr (pick_items_ne c _ t items t' hpick)
      have hti : t.items ≠ [] := by
        intro h0
        have hb : t.bulk = [] := (List.append_eq_nil_iff.mp h0).2
        have hs : t.single = [] := (List.append_eq_nil_iff.mp h0).1
        have hp : (pullOf c s.shutdown s.queue t).1 = [] := by rw [pullOf_nil c _ _ t (Or.inl hb)]
        rw [hp] at hpick
        have := (pick_none_iff c [] t).mpr ⟨hb, by simp [hs]⟩
        rw [this] at hpick; cases hpick
      have hsp := (List.filter_append_perm (fun x : Req => decide (x ∉ s.gaveUp)) items).length_eq
      simp only [List.length_append] at hsp
      have he : (fun x : Req => !decide (x ∉ s.gaveUp)) = (fun x => decide (x ∈ s.gaveUp)) := by
        funext x; simp
      rw [he] at hsp
      split
      · next haw =>
        rw [settleAll_eq]
        simp only [Pc.hand, Pc.rank, hti, ite_false, haw, List.length_nil] at hsp ⊢
        split <;> omega
      · next haw =>
        have hne' : (items.filter (fun r => r ∉ s.gaveUp)).map (·.txt) ≠ [] := by simpa using haw
        have hlen := forwardIsolated_length f dim s.calls _ hne'
        simp only [List.length_map] at hlen
        have hfst := List.map_fst_zip (l₁ := items.filter (fun r => r ∉ s.gaveUp))
          (l₂ := (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1)
          (by omega)
        have hz : (items.filter (fun r => r ∉ s.gaveUp)).zip
            (forwardIsolated f dim s.calls ((items.filter (fun r => r ∉ s.gaveUp)).map (·.txt))).1 ≠ [] := by
          intro h0; rw [h0] at hfst; exact haw hfst.symm
        rw [settleAll_eq]
        simp only [Pc.hand, Pc.rank, hti, hz, ite_false, hfst, List.length_append]
        omega

def workN (c : Cfg) (f : Fwd) (dim : Nat) : Nat → St → St
  | 0, s => s
  | n + 1, s => workN c f dim n (work c f dim s)

theorem workN_eq_run (c : Cfg) (f : Fwd) (dim : Nat) (n : Nat) (s : St) :
    workN c f dim n s = run c f dim s (List.replicate n .work) := by
  induction n generalizing s with
  | zero => rfl
  | succ n ih => simp [workN, ih, run, List.replicate_succ, step]

/-- Left alone (no new requests), the worker stops being busy within
`measure s` steps. -/
theorem workN_idle (c : Cfg) (f : Fwd) (dim : Nat) (hm : 0 < c.maxBatch) :
    ∀ n s, measure s ≤ n → ¬Busy (workN c f dim n s) := by
  intro n
  induction n with
  | zero =>
    intro s hs hb
    simp only [workN] at hb
    have := work_progress (f := f) (dim := dim) hm hb
    omega
  | succ n ih =>
    intro s hs
    by_cases hb : Busy s
    · have := work_progress (f := f) (dim := dim) hm hb
      exact ih _ (by omega)
    · simp only [workN, work_not_busy hb]
      by_cases hn : measure s ≤ n
      · exact ih s hn
      · -- not busy: the worker is a fixed point
        have : ∀ k, workN c f dim k s = s := by
          intro k; induction k with
          | zero => rfl
          | succ k ihk => simp [workN, work_not_busy hb, ihk]
        rw [this]; exact hb

theorem work_pc_ne_down {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (h : s.pc ≠ .down) :
    (work c f dim s).pc ≠ .down := by
  unfold work
  cases hpc : s.pc with
  | down => exact absurd hpc h
  | exited => simp [hpc]
  | idle =>
    simp only
    split
    · split <;> simp
    · split
      · rw [settleAll_eq]; simp
      · simp [hpc]
  | debounce b => simp
  | serving t =>
    simp only
    split
    · simp
    · split
      · rw [settleAll_eq]; simp
      · rw [settleAll_eq]; simp
  | sending t out =>
    simp only
    split
    · simp
    · rw [settle_eq, settleAll_eq]; simp
    · rw [settle_eq, settleAll_eq]; simp

theorem workN_pc_ne_down {c : Cfg} {f : Fwd} {dim : Nat} (n : Nat) {s : St} (h : s.pc ≠ .down) :
    (workN c f dim n s).pc ≠ .down := by
  induction n generalizing s with
  | zero => exact h
  | succ n ih => exact ih (work_pc_ne_down h)

/-- The worker touches only the queue, its own state, the answers and the
forward log. -/
theorem work_frame {c : Cfg} {f : Fwd} {dim : Nat} (s : St) :
    (work c f dim s).issued = s.issued ∧ (work c f dim s).shutdown = s.shutdown ∧
      (work c f dim s).gaveUp = s.gaveUp ∧ (work c f dim s).live = s.live ∧
      (work c f dim s).sup = s.sup := by
  unfold work
  split <;> (try dsimp only) <;> (repeat' split) <;> simp [settleAll_eq, settle_eq]

theorem work_shutdown {c : Cfg} {f : Fwd} {dim : Nat} {s : St} :
    (work c f dim s).shutdown = s.shutdown := (work_frame s).2.1

theorem workN_shutdown {c : Cfg} {f : Fwd} {dim : Nat} (n : Nat) (s : St) :
    (workN c f dim n s).shutdown = s.shutdown := by
  induction n generalizing s with
  | zero => rfl
  | succ n ih => simp only [workN, ih, work_shutdown]

/-- Nothing queued and nothing in the worker's hands. -/
def Rest (s : St) : Prop := s.queue = [] ∧ s.pc.hand = []

/-- While the embedder lives, every request is answered once the worker has
caught up: left alone, a running worker reaches an idle wait on an empty
queue (or its exit) within `measure s` steps, and then every request ever
queued has been observed exactly once. -/
theorem served_when_left_alone {c : Cfg} {f : Fwd} {dim : Nat} (hm : 0 < c.maxBatch) {s : St}
    (hg : Good c f dim s) (hw : 1 ≤ c.width) (hd : s.pc ≠ .down) :
    Rest (workN c f dim (measure s) s) ∧
      ∀ r ∈ s.issued, ((workN c f dim (measure s) s).obs.map Prod.fst).count r = 1 := by
  have hnb := workN_idle c f dim hm (measure s) s (Nat.le_refl _)
  have hnd := workN_pc_ne_down (c := c) (f := f) (dim := dim) (measure s) hd
  have hg' : Good c f dim (workN c f dim (measure s) s) := by
    rw [workN_eq_run]; exact good_run hw s hg _
  have hiss : (workN c f dim (measure s) s).issued = s.issued := by
    have : ∀ n s, (workN c f dim n s).issued = s.issued := by
      intro n; induction n with
      | zero => intro s; rfl
      | succ n ih => intro s; simp only [workN, ih, (work_frame s).1]
    exact this _ _
  have hrest : Rest (workN c f dim (measure s) s) := by
    unfold Busy at hnb
    cases hpc : (workN c f dim (measure s) s).pc with
    | down => exact absurd hpc hnd
    | exited => exact ⟨(hg'.exitedQ hpc).1, by simp [Pc.hand, hpc]⟩
    | idle =>
      rw [hpc] at hnb
      simp only [not_or, Decidable.not_not] at hnb
      exact ⟨hnb.1, by simp [Pc.hand, hpc]⟩
    | debounce _ => rw [hpc] at hnb; exact absurd trivial hnb
    | serving _ => rw [hpc] at hnb; exact absurd trivial hnb
    | sending _ _ => rw [hpc] at hnb; exact absurd trivial hnb
  refine ⟨hrest, ?_⟩
  intro r hr
  rw [← hiss] at hr
  exact core_at_rest hg'.toCore hrest.1 hrest.2 r hr

/-- Mirrors test `shutdown_exits_only_after_draining_queued_work`: after
shutdown is requested, a running worker exits within `measure s` steps, and
only once nothing is queued or held: every request has been answered. -/
theorem shutdown_drains {c : Cfg} {f : Fwd} {dim : Nat} (hm : 0 < c.maxBatch) {s : St}
    (hg : Good c f dim s) (hw : 1 ≤ c.width) (hd : s.pc ≠ .down) (hsd : s.shutdown = true) :
    (workN c f dim (measure s) s).pc = .exited ∧ Rest (workN c f dim (measure s) s) ∧
      ∀ r ∈ s.issued, ((workN c f dim (measure s) s).obs.map Prod.fst).count r = 1 := by
  obtain ⟨hrest, hall⟩ := served_when_left_alone hm hg hw hd
  refine ⟨?_, hrest, hall⟩
  have hnb := workN_idle c f dim hm (measure s) s (Nat.le_refl _)
  have hnd := workN_pc_ne_down (c := c) (f := f) (dim := dim) (measure s) hd
  have hs' := workN_shutdown (c := c) (f := f) (dim := dim) (measure s) s
  unfold Busy at hnb
  cases hpc : (workN c f dim (measure s) s).pc with
  | down => exact absurd hpc hnd
  | exited => rfl
  | idle => rw [hpc] at hnb; simp only [not_or] at hnb; exact absurd (hs'.trans hsd) hnb.2
  | debounce _ => rw [hpc] at hnb; exact absurd trivial hnb
  | serving _ => rw [hpc] at hnb; exact absurd trivial hnb
  | sending _ _ => rw [hpc] at hnb; exact absurd trivial hnb


/-- An accounting theorem, not liveness: whatever state the embedder is in,
request shutdown (the last handle drops), let the worker run, let the
supervisor exit and `Shared` drop, and every request ever queued has been
observed exactly once, so no request is ever left without an outcome. In a
wedged or dead state (worker `down`, supervisor gone) those outcomes are
`.dropped` from `dropShared`, which callers see as a permanent `Backend`
error, not an answer. And the recipe requests shutdown while callers may
still be waiting, which the Rust cannot reach: an awaiting `embed()` keeps a
handle alive. The liveness theorem is `served_when_left_alone`, for a
running worker (`pc ≠ down`). -/
theorem eventually_all_answered {c : Cfg} {f : Fwd} {dim : Nat} (hm : 0 < c.maxBatch)
    (hw : 1 ≤ c.width) {s : St} (hg : Good c f dim s) :
    let evs := [Ev.shutdown] ++ List.replicate (measure (step c f dim s .shutdown)) .work ++
      [.supervise true, .dropShared]
    Rest (run c f dim s evs) ∧ ∀ r ∈ s.issued, ((run c f dim s evs).obs.map Prod.fst).count r = 1 := by
  intro evs
  have hg1 := good_step hw hg .shutdown
  have hiss1 : (step c f dim s .shutdown).issued = s.issued := rfl
  have hsd1 : (step c f dim s .shutdown).shutdown = true := rfl
  generalize hs1 : step c f dim s .shutdown = s1 at hg1 hiss1 hsd1
  have hrun : run c f dim s evs =
      run c f dim (workN c f dim (measure s1) s1) [.supervise true, .dropShared] := by
    simp only [evs, run, List.foldl_append, List.foldl_cons, List.foldl_nil, hs1]
    rw [← run, ← workN_eq_run]
  rw [hrun]
  have hiss2 : (workN c f dim (measure s1) s1).issued = s1.issued := by
    have : ∀ n s, (workN c f dim n s).issued = s.issued := by
      intro n; induction n with
      | zero => intro s; rfl
      | succ n ih => intro s; simp only [workN, ih, (work_frame s).1]
    exact this _ _
  have hg2 : Good c f dim (workN c f dim (measure s1) s1) := by
    rw [workN_eq_run]; exact good_run hw _ hg1 _
  have hsd2 : (workN c f dim (measure s1) s1).shutdown = true := by
    rw [workN_shutdown]; exact hsd1
  have hnb := workN_idle c f dim hm (measure s1) s1 (Nat.le_refl _)
  generalize workN c f dim (measure s1) s1 = s2 at hiss2 hg2 hsd2 hnb
  have hpc2 : s2.pc = .down ∨ s2.pc = .exited := by
    unfold Busy at hnb
    cases hpc : s2.pc with
    | down => exact Or.inl rfl
    | exited => exact Or.inr rfl
    | idle => rw [hpc] at hnb; simp only [not_or] at hnb; exact absurd hsd2 hnb.2
    | debounce _ => rw [hpc] at hnb; exact absurd trivial hnb
    | serving _ => rw [hpc] at hnb; exact absurd trivial hnb
    | sending _ _ => rw [hpc] at hnb; exact absurd trivial hnb
  have hh2 : s2.pc.hand = [] := by rcases hpc2 with h | h <;> rw [h] <;> rfl
  -- the supervisor exits
  have hr2 : run c f dim s2 [.supervise true, .dropShared] =
      step c f dim (step c f dim s2 (.supervise true)) .dropShared := rfl
  rw [hr2]
  have hg3 := good_step hw hg2 (.supervise true)
  have h3 : (step c f dim s2 (.supervise true)).pc = s2.pc ∧
      (step c f dim s2 (.supervise true)).sup = false ∧
      (step c f dim s2 (.supervise true)).queue = s2.queue ∧
      (step c f dim s2 (.supervise true)).live = s2.live ∧
      (step c f dim s2 (.supervise true)).shutdown = true ∧
      (step c f dim s2 (.supervise true)).issued = s2.issued := by
    simp only [step]
    split
    · simp [hsd2]
    · next h =>
      have : s2.sup = false := by
        cases hs : s2.sup
        · rfl
        · exact absurd ⟨hs, hpc2⟩ h
      exact ⟨rfl, this, rfl, rfl, hsd2, rfl⟩
  generalize step c f dim s2 (.supervise true) = s3 at hg3 h3
  obtain ⟨hpc3, hsup3, hq3, hl3, hsd3, hiss3⟩ := h3
  have hg4 := good_step hw hg3 .dropShared
  have hrest : Rest (step c f dim s3 .dropShared) := by
    simp only [step]
    split
    · rw [settleAll_eq]; refine ⟨rfl, ?_⟩; simp [hpc3, hh2]
    · next h =>
      have hlive : s3.live = false := by
        cases hl : s3.live
        · rfl
        · exact absurd ⟨hl, hsd3, hsup3, hpc3 ▸ hpc2⟩ h
      exact ⟨(hg3.deadQ hlive).1, by rw [hpc3]; exact hh2⟩
  have hiss4 : (step c f dim s3 .dropShared).issued = s3.issued := by
    simp only [step]; split
    · rw [settleAll_eq]
    · rfl
  refine ⟨hrest, ?_⟩
  intro r hr
  have : r ∈ (step c f dim s3 .dropShared).issued := by
    rw [hiss4, hiss3, hiss2, hiss1]; exact hr
  exact core_at_rest hg4.toCore hrest.1 hrest.2 r this


/-! ## No cross-wiring, and isolation over whole runs -/

/-- Where a new observation comes from: a caller's own timeout, a dropped
sender, or an answer being sent, which is the `out` entry of its own
request. -/
theorem step_obs_new {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (e : Ev) :
    ∀ p ∈ (step c f dim s e).obs, p ∈ s.obs ∨ p.2 = .dropped ∨ p.2 = .timedOut ∨
      ∃ t out, s.pc = .sending t out ∧ ∃ o ∈ out, p = (o.1, .res o.2) := by
  intro p hp
  cases e with
  | enqueue r => simp only [step] at hp; split at hp <;> exact Or.inl hp
  | enqueueMany rs => simp only [step] at hp; split at hp <;> exact Or.inl hp
  | giveUp r =>
    simp only [step] at hp; split at hp
    · rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp at h; subst h; exact Or.inr (Or.inr (Or.inl rfl))
    · exact Or.inl hp
  | shutdown => exact Or.inl hp
  | die =>
    simp only [step] at hp; split at hp
    · rw [settleAll_eq] at hp
      rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp only [List.mem_map] at h; obtain ⟨_, _, rfl⟩ := h; exact Or.inr (Or.inl rfl)
    · exact Or.inl hp
  | supervise ok =>
    simp only [step] at hp
    split at hp
    · split at hp
      · exact Or.inl hp
      · split at hp
        · split at hp <;> exact Or.inl hp
        · exact Or.inl hp
    · exact Or.inl hp
  | dropShared =>
    simp only [step] at hp; split at hp
    · rw [settleAll_eq] at hp
      rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp only [List.mem_map] at h; obtain ⟨_, _, rfl⟩ := h; exact Or.inr (Or.inl rfl)
    · exact Or.inl hp
  | work =>
    simp only [step] at hp
    unfold work at hp
    cases hpc : s.pc with
    | down => simp only [hpc] at hp; exact Or.inl hp
    | exited => simp only [hpc] at hp; exact Or.inl hp
    | idle =>
      simp only [hpc] at hp
      split at hp
      · split at hp <;> exact Or.inl hp
      · split at hp
        · next hq _ =>
          have hq' : s.queue = [] := by simpa using hq
          rw [hq', settleAll_eq] at hp; simp at hp; exact Or.inl hp
        · exact Or.inl hp
    | debounce b => simp only [hpc] at hp; exact Or.inl hp
    | serving t =>
      simp only [hpc] at hp
      split at hp
      · exact Or.inl hp
      · split at hp <;>
        · rw [settleAll_eq] at hp
          rcases List.mem_append.mp hp with h | h
          · exact Or.inl h
          · simp only [List.mem_map] at h; obtain ⟨_, _, rfl⟩ := h; exact Or.inr (Or.inl rfl)
    | sending t out =>
      simp only [hpc] at hp
      split at hp
      · exact Or.inl hp
      · next o =>
        rw [settle_eq, settleAll_eq] at hp
        rcases List.mem_append.mp hp with h | h
        · exact Or.inl h
        · simp only [List.mem_map, List.mem_filter, List.mem_singleton] at h
          obtain ⟨_, ⟨rfl, _⟩, rfl⟩ := h
          exact Or.inr (Or.inr (Or.inr ⟨t, [o], rfl, o, by simp, rfl⟩))
      · next o os _ =>
        rw [settle_eq, settleAll_eq] at hp
        rcases List.mem_append.mp hp with h | h
        · exact Or.inl h
        · simp only [List.mem_map, List.mem_filter, List.mem_singleton] at h
          obtain ⟨_, ⟨rfl, _⟩, rfl⟩ := h
          exact Or.inr (Or.inr (Or.inr ⟨t, o :: os, rfl, o, by simp, rfl⟩))

/-- Every vector or error a caller is answered with is its own text's. -/
def IsoObs (row : Txt → Option Vec) (dim : Nat) (s : St) : Prop :=
  ∀ r x, (r, Obs.res x) ∈ s.obs → x = soloRes row dim r.txt

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

/-- With a row-wise forward of fixed width and a canary that embeds, an
answer being sent is its own request's solo answer. -/
theorem sending_own (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) {c : Cfg} {s : St} (hg : Good c (rowFwd row) dim s)
    {t : Take} {out : List (Req × Res)} (hpc : s.pc = .sending t out) :
    ∀ o ∈ out, o.2 = soloRes row dim o.1.txt := by
  obtain ⟨pre, k, hwd⟩ := hg.wired t out hpc
  intro o ho
  by_cases hne : (pre ++ out).map (·.1.txt) = []
  · simp at hne; rw [hne.2] at ho; simp at ho
  · rw [forwardIsolated_rowwise row dim w k hu hc _ hne, List.map_map] at hwd
    exact map_eq_pointwise hwd o (List.mem_append_right _ ho)

theorem iso_step (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) {c : Cfg} {s : St} (hg : Good c (rowFwd row) dim s)
    (hi : IsoObs row dim s) (e : Ev) : IsoObs row dim (step c (rowFwd row) dim s e) := by
  intro r x hx
  rcases step_obs_new e _ hx with h | h | h | ⟨t, out, hpc, o, ho, hp⟩
  · exact hi r x h
  · cases h
  · cases h
  · simp only [Prod.mk.injEq, Obs.res.injEq] at hp
    obtain ⟨rfl, rfl⟩ := hp
    exact sending_own row dim w hu hc hg hpc o ho

/-- #125 over whole runs, through reordering by length, the per-text
reruns, interleaved arrivals, timeouts, deaths and restarts: with a row-wise
forward of fixed width and a canary that embeds, every answer a caller
receives is exactly what its own text gets alone. No caller ever receives
another caller's vector, and no text's failure reaches another caller. -/
theorem answers_are_own (row : Txt → Option Vec) (dim w : Nat) (hu : Uniform row w)
    (hc : (row canary).isSome) {c : Cfg} (hw : 1 ≤ c.width) (evs : List Ev) :
    IsoObs row dim (run c (rowFwd row) dim St.init evs) := by
  suffices ∀ s, Good c (rowFwd row) dim s → IsoObs row dim s →
      IsoObs row dim (run c (rowFwd row) dim s evs) from
    this _ (good_init c _ dim) (by intro r x hx; simp [St.init] at hx)
  induction evs with
  | nil => intro s _ hi; exact hi
  | cons e es ih =>
    intro s hg hi
    exact ih _ (good_step hw hg e) (iso_step row dim w hu hc hg hi e)

/-- No caller is answered twice, in any run. -/
theorem answered_at_most_once {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev)
    (r : Req) : ((run c f dim St.init evs).obs.map Prod.fst).count r ≤ 1 :=
  (good_reachable f dim hw evs).obsUniq r

/-- Every forward the device runs, in any run, holds at most `FORWARD_WIDTH`
texts, each within the length slack of every other. -/
theorem forwards_capped {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev) :
    ∀ b ∈ (run c f dim St.init evs).fwds, b.length ≤ c.width ∧ Slack c b :=
  (good_reachable f dim hw evs).fwdCaps

/-- A take, topped up, holds at most `MAX_BATCH` requests. -/
theorem topUp_cap (b q : List Req) (n : Nat) (h : b.length ≤ n) :
    (b ++ (takePrioritized q (n - b.length)).1).length ≤ n := by
  have := takePrioritized_length_le q (n - b.length)
  simp only [List.length_append]; omega


/-! ## The tests' scenarios, run through the state machine -/

/-- A single request (`embed`) and a bulk one (`embed_batch`) with a text of
`len` bytes. -/
def sReq (id len : Nat) : Req := ⟨id, ⟨len, id⟩, false⟩
def bReq (id len : Nat) : Req := ⟨id, ⟨len, id⟩, true⟩

/-- A forward that embeds every text: `fake 4` on texts with no bad tag. -/
def okFwd : Fwd := fake 4

/-- `a_take_serves_single_embeds_before_bulk_ones_in_arrival_order`. -/
theorem test_take_singles_first :
    (takePrioritized [bReq 1 2, bReq 2 2, sReq 3 2, bReq 4 2, sReq 5 2] 3).1 =
        [sReq 3 2, sReq 5 2, bReq 1 2] ∧
      (takePrioritized [bReq 1 2, bReq 2 2, sReq 3 2, bReq 4 2, sReq 5 2] 3).2 = [bReq 2 2, bReq 4 2] := by
  decide

/-- `initial_take_never_exceeds_max_batch_and_leftovers_survive`: 50 queued,
a take of 32, 18 left queued in order. -/
theorem test_initial_take (q : List Req) (hq : q.length = 50) :
    (takePrioritized q Cfg.rust.maxBatch).1.length = 32 ∧
      (takePrioritized q Cfg.rust.maxBatch).2.length = 18 := by
  have h1 := takePrioritized_length q Cfg.rust.maxBatch
  have h2 := (takePrioritized_perm q Cfg.rust.maxBatch).length_eq
  simp only [List.length_append, hq, Cfg.rust] at h1 h2 ⊢
  omega

def tenSingles : List Ev := (List.range 10).map fun i => .enqueue (sReq i 2)
def thirtyMore : List Ev := (List.range 30).map fun i => .enqueue (sReq (100 + i) 2)

set_option maxRecDepth 20000 in
/-- `debounce_top_up_takes_only_the_remaining_capacity`: a take of 10, then
30 more arrive during the debounce; the top-up takes 22 and leaves 8. -/
theorem test_top_up :
    let s := run Cfg.rust okFwd 4 St.init
      ([.supervise true] ++ tenSingles ++ [.work] ++ thirtyMore ++ [.work])
    s.pc.hand.length = 32 ∧ s.queue.length = 8 := by decide

def bulk24 : List Req := (List.range 24).map fun i => bReq i 11

/-- `a_single_embed_queued_during_a_take_waits_for_one_forward_only`: a take
of 24 bulk texts; a recall arrives while the first forward runs; the next
forward is the recall's. -/
theorem test_recall_next_forward :
    let s := run Cfg.rust okFwd 4 { St.init with pc := .serving (split bulk24), issued := bulk24 }
      ([.work, .enqueue (sReq 100 12)] ++ List.replicate 40 .work)
    s.fwds.map List.length = [8, 1, 8, 8] ∧ s.fwds[1]? = some [(sReq 100 12).txt] ∧
      s.obs.length = 25 := by decide

def bulk16 : List Req := (List.range 16).map fun i => bReq i 11

/-- The forwards of `a_stream_of_single_embeds_does_not_starve_a_takes_bulk_texts`:
16 bulk texts, a new recall arriving during each forward. -/
def streamEvs : List Ev :=
  [.work, .enqueue (sReq 100 6)] ++ List.replicate 8 .work ++
  [.work, .enqueue (sReq 101 6), .work] ++
  [.work, .enqueue (sReq 102 6), .work] ++
  [.work, .enqueue (sReq 103 6), .work] ++
  [.work, .enqueue (sReq 104 6)] ++ List.replicate 8 .work ++
  [.work, .work, .work]

/-- The bulk chunks run at forwards 0 and `1 + MAX_SINGLE_STREAK`, and the
take ends after six forwards. Shown about the docs: the recall that
arrives during the third single forward (`sReq 103`) waits for that forward
and then for a bulk forward before its own, more than `serve_take`'s "waits
for at most the one forward in flight". This is the mild case; a long recall
overtaken by shorter later singles waits out the whole take, beyond
`MAX_SINGLE_STREAK`'s "at most one bulk forward more" too
(`long_recall_waits_out_the_take`). -/
theorem test_stream_of_singles :
    let s := run Cfg.rust okFwd 4 { St.init with pc := .serving (split bulk16), issued := bulk16 } streamEvs
    s.fwds.map List.length = [8, 1, 1, 1, 8, 1] ∧
      s.fwds[4]? = some (bulk16.drop 8 |>.map (·.txt)) ∧
      s.fwds[5]? = some [(sReq 103 6).txt] ∧ s.pc = .idle ∧ s.queue = [sReq 104 6] := by
  decide

/-- `a_take_pulls_no_new_singles_once_shutdown_is_requested`. -/
theorem test_no_pull_after_shutdown :
    let s := run Cfg.rust okFwd 4
      { St.init with queue := [sReq 1 13], issued := [sReq 1 13, bReq 2 9], shutdown := true,
                     pc := .serving (split [bReq 2 9]) } [.work, .work, .work]
    s.fwds = [[(bReq 2 9).txt]] ∧ s.queue = [sReq 1 13] ∧ s.pc = .idle := by decide

/-- `a_request_whose_caller_gave_up_is_not_forwarded`. -/
theorem test_gave_up_not_forwarded :
    let s := run Cfg.rust okFwd 4 St.init
      [.supervise true, .enqueueMany [bReq 1 4, bReq 2 4], .giveUp (bReq 1 4), .work, .work, .work, .work]
    s.fwds = [[(bReq 2 4).txt]] ∧
      s.obs = [(bReq 1 4, .timedOut), (bReq 2 4, .res (.vec ⟨4, 0⟩))] := by decide

/-! ## What the model shows about the Rust -/

/-- A worker death answers the requests in its hands with a dropped sender,
which `recv_bounded` and `embed_batch` report as
`Backend("candle coalescer dropped the request (worker exited mid-batch)")`:
permanent, so a write's text is refused as `LamboError::Embed` and its
durable intent is consumed `failed`, although nothing was learned about the
text (here a text that embeds). `forward_isolated` classes a failure that is
not the text's as `Unavailable`; the death path does not. The supervisor's
own doc says a worker exit "should be impossible", so this is a latent
mismatch on a defensive path. -/
theorem worker_death_permanent :
    let s := run Cfg.rust okFwd 4 St.init [.supervise true, .enqueue (sReq 1 7), .work, .die]
    s.obs = [(sReq 1 7, .dropped)] ∧ Obs.transient .dropped = some false ∧
      soloRes (fun t => some ⟨4, t.len % 4⟩) 4 (sReq 1 7).txt = .vec ⟨4, 3⟩ := by decide

/-- The queue survives a worker death: the supervisor restarts the worker
and the queued request is served. -/
theorem restart_serves_queue :
    let s := run Cfg.rust okFwd 4 St.init
      ([.supervise true, .enqueue (sReq 1 7), .work, .enqueue (sReq 2 5), .die, .supervise true] ++
        List.replicate 6 .work)
    s.obs = [(sReq 1 7, .dropped), (sReq 2 5, .res (.vec ⟨4, 1⟩))] ∧ s.pc = .idle := by decide

/-- After shutdown the supervisor never restarts a dead worker
(`if shared.queue.shutdown_requested() { break; }`), so what is still
queued is never forwarded; it is answered only when `Shared` drops, as a
dropped sender. Harmless as the Rust stands: shutdown comes from the last
handle's drop, and an awaiting `embed()` borrows a handle, so no caller is
waiting by then. -/
theorem death_after_shutdown_abandons_queue :
    let s := run Cfg.rust okFwd 4 St.init
      [.supervise true, .enqueue (sReq 1 7), .work, .enqueue (sReq 2 5), .shutdown, .die,
        .supervise true, .work, .dropShared]
    s.fwds = [] ∧ s.obs = [(sReq 1 7, .dropped), (sReq 2 5, .dropped)] ∧ s.sup = false := by decide

/-- A failed spawn kills the supervisor (`expect("spawn candle coalescer
worker")`): from then on no event but `shutdown` can answer a request
except by the caller's own timeout. `EMBED_WAIT` turns this wedge into a
named `Unavailable` after ten minutes, every time. -/
def Wedged (s : St) : Prop := s.sup = false ∧ s.pc = .down ∧ s.shutdown = false

theorem wedged_stays {c : Cfg} {f : Fwd} {dim : Nat} {s : St} (hw : Wedged s) (e : Ev)
    (he : e ≠ .shutdown) :
    Wedged (step c f dim s e) ∧ ∀ p ∈ (step c f dim s e).obs, p ∈ s.obs ∨ p.2 = .timedOut := by
  obtain ⟨hsup, hpc, hsd⟩ := hw
  cases e with
  | shutdown => exact absurd rfl he
  | enqueue r =>
    simp only [step]; split
    · exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
    · exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
  | enqueueMany rs =>
    simp only [step]; split
    · exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
    · exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
  | giveUp r =>
    simp only [step]; split
    · refine ⟨⟨hsup, hpc, hsd⟩, fun p hp => ?_⟩
      rcases List.mem_append.mp hp with h | h
      · exact Or.inl h
      · simp at h; subst h; exact Or.inr rfl
    · exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
  | work =>
    simp only [step, work, hpc]
    exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
  | die =>
    simp only [step, hpc, Pc.running]
    exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
  | supervise ok =>
    simp only [step, hsup]
    exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩
  | dropShared =>
    have : step c f dim s .dropShared = s := by simp [step, hsd]
    rw [this]
    exact ⟨⟨hsup, hpc, hsd⟩, fun p hp => Or.inl hp⟩

theorem test_spawn_failure_wedges :
    let s := run Cfg.rust okFwd 4 St.init
      ([.supervise false, .enqueue (sReq 1 7)] ++ List.replicate 5 .work ++ [.giveUp (sReq 1 7)])
    s.fwds = [] ∧ s.obs = [(sReq 1 7, .timedOut)] ∧ s.sup = false ∧ s.pc = .down ∧
      s.shutdown = false := by decide

/-- Queued bulk work can starve: a take holds `MAX_BATCH` requests and takes
singles first, so while at least `MAX_BATCH` single embeds are queued at
every take, a queued bulk request is never taken (`takePrioritized_no_bulk`).
The per-take bound (`take_ends`, `pick_progress`) protects only the bulk
texts already in a take. Shown here with `MAX_BATCH = 2`: a write's text is
queued, then two recalls arrive before each take; after three takes the
write's text is still queued and has never been forwarded. -/
def cfg2 : Cfg := ⟨2, 8, 64, 3⟩

def starveEvs : List Ev :=
  [.supervise true, .enqueueMany [bReq 0 9], .enqueue (sReq 1 5), .enqueue (sReq 2 5)] ++
    List.replicate 4 .work ++ [.enqueue (sReq 3 5), .enqueue (sReq 4 5)] ++
    List.replicate 4 .work ++ [.enqueue (sReq 5 5), .enqueue (sReq 6 5)] ++
    List.replicate 4 .work

theorem queued_bulk_starves :
    let s := run cfg2 okFwd 4 St.init starveEvs
    s.queue = [bReq 0 9] ∧ s.fwds.length = 3 ∧ (bReq 0 9).txt ∉ s.fwds.flatten := by decide


/-- The serving step's pulls obey `drive`'s discipline: at most `MAX_BATCH`
singles, and none once the take has no bulk texts, so `take_ends` bounds
every take of the state machine. -/
theorem pullOf_drive (c : Cfg) (sd : Bool) (q : List Req) (t : Take) :
    (pullOf c sd q t).1.length ≤ c.maxBatch ∧ (t.bulk = [] → (pullOf c sd q t).1 = []) :=
  ⟨pullOf_length c sd q t, fun h => by rw [pullOf_nil c sd q t (Or.inl h)]⟩

/-- No cross-wiring for any forward: in every run, the answers still to be
sent (`out`) are a suffix of a list whose answers are `forward_isolated`'s
results, position by position, on that list's own texts. `pre` is
existential: the invariant does not record that it is the part of the same
forward already sent, nor which requests it held, so the theorem pins each
pending answer to its own request's position in one `forward_isolated`
call, and says nothing about requests already answered (those are covered,
for a row-wise forward, by `answers_are_own`). -/
theorem answers_wired {c : Cfg} (f : Fwd) (dim : Nat) (hw : 1 ≤ c.width) (evs : List Ev)
    (t : Take) (out : List (Req × Res)) (h : (run c f dim St.init evs).pc = .sending t out) :
    ∃ pre k, (pre ++ out).map Prod.snd = (forwardIsolated f dim k ((pre ++ out).map (·.1.txt))).1 :=
  (good_reachable f dim hw evs).wired t out h


/-- The reviewer's scenario: a take of 24 bulk texts of 11 bytes; a 200-byte
recall (`longR`) arrives during the first bulk forward; then a 5-byte single
arrives every two worker steps. -/
def longR : Req := sReq 100 200

def overtakeEvs : List Ev :=
  [.work, .enqueue longR] ++
    ((List.range 40).map (fun i => List.replicate 2 Ev.work ++ [.enqueue (sReq (200 + i) 5)])).flatten ++
    List.replicate 60 .work

set_option maxRecDepth 20000 in
/-- Shown about the Rust: a long recall waits out the take. Shorter singles
that arrive later keep overtaking it (length chunks, shortest first, pulled
before every forward while bulk remains), so it runs at forward 10, after
all three bulk forwards, not after "the one forward in flight" nor "at most
one bulk forward more". -/
theorem long_recall_waits_out_the_take :
    let s := run Cfg.rust okFwd 4 { St.init with pc := .serving (split bulk24), issued := bulk24 }
      overtakeEvs
    s.fwds.findIdx? (fun b => b.any (·.tag == 100)) = some 10 ∧
      ((s.fwds.take 10).filter (fun b => b.any (·.len == 11))).length = 3 := by
  decide

end Lambo.BatchQueue
