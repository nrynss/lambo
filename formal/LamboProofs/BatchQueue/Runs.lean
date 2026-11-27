import LamboProofs.BatchQueue.Worker

/-
# The candle embedder's batch queue: the tests' scenarios

Runs of `Worker.lean`'s state machine, checked by `decide`, that mirror the
coalescer tests in `src/embed/candle.rs` (`test_take_singles_first`,
`test_initial_take`, `test_top_up`, `test_recall_next_forward`,
`test_stream_of_singles`, `test_no_pull_after_shutdown`,
`test_gave_up_not_forwarded`, `test_spawn_failure_wedges`), and the runs that
show what the Rust does.

Results about the Rust: a worker death answers the requests in its hands
`Unavailable` ("worker exited mid-batch"), transient, as `forward_isolated`
classes a failure that is not the text's (`worker_death_transient`, since
#309; the path is defensive). The supervisor
restarts the worker and the queue is served (`restart_serves_queue`), but not
after shutdown, when only the drop of `Shared` answers the queue
(`death_after_shutdown_abandons_queue`, harmless: no caller can be waiting);
after a failed spawn only timeouts answer (`wedged_stays`). With `MAX_BATCH`
singles queued at every take a queued bulk text is never taken
(`queued_bulk_starves`). A recall can wait for the whole rest of its take:
singles are length-chunked shortest first and pulled before every forward
while bulk texts remain, so shorter singles keep overtaking a long recall
(`long_recall_waits_out_the_take`: a 200-byte recall runs at forward 10, after
all three bulk forwards); the take still ends (`take_ends`). Since #311 the
docs of `take_prioritized`, `serve_take` and `MAX_SINGLE_STREAK` state both.
Before, they said a write's batch was never starved, that a recall "waits for
at most the one forward in flight", and that it "waits at most one bulk
forward more".

Boundary: each run is one interleaving with the Rust's constants or a small
configuration (`cfg2`); the general statements are `Worker.lean`'s and
`Pure.lean`'s.
-/

namespace Lambo.BatchQueue

open Lambo.Lib (eraseIdx_perm map_eq_pointwise)

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
take ends after six forwards. The recall that arrives during the third single
forward (`sReq 103`) waits for that forward and then for a bulk forward
before its own, the wait `MAX_SINGLE_STREAK`'s doc gives a recall that is
waiting when the streak ends. This is the mild case; a long recall overtaken
by shorter later singles waits out the whole take
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

/-- Since #309 a worker death is transient for the requests the worker held:
each caller sees a dropped sender, which `recv_bounded` and `embed_batch`
report as `worker_exited()`, `Unavailable`, so a write's text is refused
retryably (`LamboError::EmbedUnavailable`) and not as a refusal of the text.
The Rust test is `a_worker_death_answers_the_requests_it_held_as_transient`. -/
theorem worker_death_transient :
    let s := run Cfg.rust okFwd 4 St.init [.supervise true, .enqueue (sReq 1 7), .work, .die]
    s.obs = [(sReq 1 7, .dropped)] ∧ Obs.transient .dropped = some true := by decide

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

/-- Once the embedder worker is wedged (its spawn failed), every event but
shutdown keeps it wedged, and callers only ever time out (`supervise`). -/
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

/-- A failed worker spawn wedges the embed queue: `supervise` cannot restart
it and queued callers time out. -/
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

/-- Queued bulk work can starve: while `MAX_BATCH` singles keep arriving,
`take_prioritized` never takes a queued bulk request. -/
theorem queued_bulk_starves :
    let s := run cfg2 okFwd 4 St.init starveEvs
    s.queue = [bReq 0 9] ∧ s.fwds.length = 3 ∧ (bReq 0 9).txt ∉ s.fwds.flatten := by decide


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
all three bulk forwards. `serve_take`'s doc cites this run for a recall's
wait (#311). -/
theorem long_recall_waits_out_the_take :
    let s := run Cfg.rust okFwd 4 { St.init with pc := .serving (split bulk24), issued := bulk24 }
      overtakeEvs
    s.fwds.findIdx? (fun b => b.any (·.tag == 100)) = some 10 ∧
      ((s.fwds.take 10).filter (fun b => b.any (·.len == 11))).length = 3 := by
  decide

end Lambo.BatchQueue
