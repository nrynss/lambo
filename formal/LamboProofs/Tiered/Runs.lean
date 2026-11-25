import LamboProofs.Tiered.Machine

/-
# Runs of the tiered store, from the tests

Concrete runs of `Machine.lean`'s state machine, checked by `decide`, that
mirror the tests in `src/store/tiered/tests.rs`: the mirror and served leg,
the contract refusal, the mirror failure and repair, the #18 cases
(overtaken repair, lost lease, erase, ahead marker, unleased flush), the #293
re-take, the #319 migration and relabel, and the #320 mirror fence. Other
runs show known disagreements with the intended behavior. The same batches
before #319, and the fixes #319 did not take, are `History.lean`, which the
docs may not cite.

Results about the Rust: a re-embed migration (`Graph::reembed_all`, every
`UpsertNode` before the trailing `SetEmbedding`) switches the contract, so
the mirror holds it back and a repair re-indexes it from the durable snapshot
(`reembed_migration_held_back`, `reembed_migration_repaired`). An in-place
re-embed lets a
read rank two builds together while the bulk lands
(`inplace_reembed_mixes_builds`). `sync_for_read` re-checks only `Unknown` and
`Stale`, so a reader that once found the marker in sync trusts the index until
it reloads (`reader_never_rechecks`; real readers bound the window by reload
or by being one-shot or pinned). Before #320, a mirror stalled past its
lease could write an erased session's vector and marker back
(`mirror_lands_after_erase`). The #320 runs use `fencedMirrorTick` to model
a durable lease check at each request submission. A bulk already submitted
can still land; the separate check prevents the next marker submission
(`mirror_does_not_submit_after_erase`,
`mirror_bulk_in_flight_does_not_recreate_marker`). `repair_once`
reconciles from the older snapshot when `recheck_ahead` finds the marker
behind, contrary to #18's rule (`repair_from_pre_recheck_snapshot`). A batch
of only `DeleteEdge`s leaves the marker behind
(`edge_only_batch_leaves_marker_behind`). `next_version` restarts the counter
on any other token, an older one included, so a stale repair pass rewinds the
version and leaves the session in sync serving an old vector for good
(`stale_pass_rewinds_version`).

Boundary: each run is one interleaving; the general statements are
`Machine.lean`'s. `mirror_lands_after_erase` needs the writer's lease to have
lapsed first, since the real `erase_gate` refuses while it is live.
-/

namespace Lambo.Tiered
/-! ## Runs from the tests -/

section Runs

/-- The tests' tier: repairs without waiting (`with_repair_backoff(ZERO)`). -/
def Pt : Params := { backoff := 0 }

/-- What a read returned, at some nodes: `none` when it was not the index. -/
def Answer.at (a : Answer) (ns : List Nat) : Option (List (Option Vec)) :=
  match a with
  | .index _ view => some (ns.map view)
  | _ => none

def Answer.isFallback : Answer → Bool
  | .fallback => true
  | _ => false

def answerAt (s : St) (ns : List Nat) : Option (List (Option Vec)) :=
  s.answer.bind (·.at ns)

def fellBack (s : St) : Bool := s.answer.any (·.isFallback)

/-- The index document of node `n` in contract `c`. -/
def docAt (s : St) (c n : Nat) : Option Vec := (s.rt c n).doc

def markerEpoch (s : St) : Option Nat := s.mark.map (·.2)

/-- Acquire, then load (`attach`, the order `Memory` attaches in). -/
def attachEvs : List Ev := [.acquire, .loadBegin, .loadEnd]

/-- `seed_batch`: contract 1, two vectors of build 0, one concept without. -/
def seed : List Mut := [.setContract (some 1), .upsert 1 (some (0, 10)), .upsert 2 (some (0, 20)),
  .upsert 3 none]

/-- Attached, seeded and mirrored: in sync, the index serves nodes 1 and 2. -/
def seeded : St := run Pt {} (attachEvs ++ [.write seed] ++ steps .mirStep 5)

/-- `a_committed_flush_is_mirrored_and_the_index_serves_the_vector_leg`. -/
example : seeded.tier.sync = .inSync ∧ markerEpoch seeded = some 1 ∧
    answerAt (run Pt seeded [.read 1]) [1, 2, 3] = some [some (0, 10), some (0, 20), none] := by
  decide

/-- `a_contract_mismatch_is_refused_...`: a read expecting another contract is
refused. -/
example : (match (run Pt seeded [.read 2]).answer with | some .refused => true | _ => false) = true := by
  decide

/-- `a_mirror_failure_keeps_the_flush_and_the_next_flush_repairs`: the flush
is durable while the index is down, reads fall back, and the next flush after
the index returns repairs the session in full. -/
def outage : St := run Pt {} (attachEvs ++ [.setDown true, .write seed, .mirStep, .setDown false])

example : outage.d.vecs.length = 2 ∧ outage.tier.sync = .stale ∧ outage.tier.failures = 1 ∧
    fellBack (run Pt outage [.read 1]) = true := by decide

def outageRepaired : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 20 ++ [.read 1])

example : outageRepaired.tier.sync = .inSync ∧ markerEpoch outageRepaired = some 2 ∧
    answerAt outageRepaired [1, 2, 3, 4] = some [some (0, 10), some (0, 20), none, some (0, 40)] ∧
    outageRepaired.rep = .idle := by decide

/-- `repair_converges` applies to the outage test: after the index came back
and the next flush asked for a repair, the task is at its load with every
hypothesis met, and the theorem gives the match. -/
def atLoad : St := run Pt outage [.write [.upsert 4 (some (0, 40))], .repStep]

example : atLoad.rep = .load 1 ∧ atLoad.tier.held = some 1 ∧ atLoad.d.lease = 1 ∧
    atLoad.d.row = .ours ∧ atLoad.d.erased = false ∧ atLoad.down = false ∧
    checkMarker false atLoad.mark (some atLoad.d) = .behind ∧
    nextVersion atLoad.tier.tok atLoad.tier.cnt 1 = some (1, 2, shift + 2) ∧
    atLoad.mark = none ∧ atLoad.tier.committed ≤ atLoad.d.epoch ∧ atLoad.tier.again = false ∧
    atLoad.d.vecs.length = 3 := by decide

theorem atLoad_rt : atLoad.rt = Idx.empty := rfl

example : Matches (run Pt atLoad (steps .repStep (2 * 3 + 9))) :=
  (repair_converges Pt atLoad 1 1 2 (shift + 2) (by decide) (by decide) (by decide)
    (by decide) (by decide) (by decide) (by decide) (by decide)
    (by intro c n; rw [atLoad_rt]; simp [Idx.empty, shift])
    (by intro w e h; simp [show atLoad.mark = none by decide] at h)
    (by decide) (by decide)).1

/-- `repairs_run_one_at_a_time_per_session` and the #18 test
(`a_repair_overtaken_by_a_flush_is_not_trusted_until_the_rerun`): a flush that
commits while a repair runs is not in its snapshot; the pass writes its own
epoch but does not mark the session in sync, a read falls back, and the rerun
settles it. -/
def overtaken : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 12 ++
    [.write [.upsert 5 (some (0, 50))]] ++ steps .repStep 4)

example : overtaken.rep = .head ∧ markerEpoch overtaken = some 2 ∧ overtaken.tier.sync ≠ .inSync ∧
    overtaken.tier.repairing = true ∧ fellBack (run Pt overtaken [.read 1]) = true := by decide

example : (run Pt overtaken (steps .repStep 30)).tier.sync = .inSync ∧
    markerEpoch (run Pt overtaken (steps .repStep 30)) = some 3 ∧
    docAt (run Pt overtaken (steps .repStep 30)) 1 5 = some (0, 50) := by decide

/-- `a_repair_that_lost_its_lease_stops_writing` (#18): the holder's repair
lands its first chunk, the lease lapses and another process erases the
session; the repair's next fence check stops it. One write landed, no marker,
stale, and the lost lease is not counted as an index failure. -/
def lostLease : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 6 ++ [.eraseOther] ++
    steps .repStep 10)

example : lostLease.log.filter (· = .repair 1) = [.repair 1] ∧ lostLease.mark = none ∧
    lostLease.tier.sync = .stale ∧ lostLease.tier.failures = 1 ∧ lostLease.tier.held = none ∧
    lostLease.rep = .idle := by decide

/-- `erase_stops_this_stores_repair_before_it_sweeps` (#18): an erase through
this store fences the repair, waits for it, then sweeps: nothing of the
session is left and the state is gone. -/
def erasedHere : St :=
  run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 6 ++ [.eraseStop] ++
    steps .repStep 10 ++ [.eraseFinish])

example : erasedHere.mark = none ∧ docAt erasedHere 1 1 = none ∧ docAt erasedHere 1 4 = none ∧
    erasedHere.tier = {} ∧ erasedHere.erasing = false := by decide

/-- `a_flush_is_not_mirrored_after_the_lease_was_lost` and the #293 review
(`a_refresh_that_retakes_the_lease_does_not_adopt_its_token`): after the
operator override the heartbeat's refresh re-takes the row at a new token, the
store stops calling the lease held, the writer's next flush is refused by the
primary, and no repair runs. -/
def retaken : St := run Pt seeded [.override, .refresh]

example : retaken.d.lease = 2 ∧ retaken.tier.held = none ∧ retaken.wtok = some 1 ∧
    (run Pt retaken [.write [.upsert 4 (some (0, 40))]]).d = retaken.d := by decide

/-- `a_marker_ahead_of_the_loaded_snapshot_is_rechecked_not_repaired` (#18): a
load that read e1 while the holder's own flush committed and mirrored e2 sees
the marker ahead, re-checks against a fresh load, and is in sync without a
repair. -/
example :
    let s := run Pt seeded ([.loadBegin, .write [.upsert 4 (some (0, 40))]] ++ steps .mirStep 3 ++
      [.loadEnd])
    s.tier.sync = .inSync ∧ s.rep = .idle ∧ s.tier.repairing = false := by decide

/-- `an_unleased_flush_is_not_mirrored_and_the_next_holder_repairs` (#18). -/
example :
    let s := run Pt {} [.writeUnleased seed]
    s.tier.sync = .stale ∧ s.log = [] ∧
      (run Pt s (attachEvs ++ steps .repStep 20)).tier.sync = .inSync := by decide

/-! ### The #319 re-embed batch -/

/-- **A re-embed migration through the tier wrote its new vectors into the
old contract's index** (the previous version, #319). `Graph::reembed_all`
drains every `UpsertNode` **before** the trailing `SetEmbedding` (so the
durable flush is one old-to-new transaction), but `project` switches the
contract only for what follows a `SetEmbedding`: the rewrite is projected
into contract 1's index under contract 1's hash, the new contract 2's index
gets nothing, the marker advances and the session stays in sync. A read under
the new contract is served from the index and finds no vector at all, and
since the marker is in sync no load ever repairs it (only
`lambo recall-index backfill` would); contract 1's index now holds build-1
vectors under contract 1's name, which a reader still holding contract 1 ranks
a contract-1 probe against. `project` itself is unchanged by the fix. -/
def reembedBatch : List Mut := [.upsert 1 (some (1, 11)), .upsert 2 (some (1, 21)), .setContract (some 2)]

/-! ### The fix for #319 -/

/-- The issue's batch, and the relabel, switch; the seed (first stamp first)
and the in-place rewrite (no `SetEmbedding`) do not. -/
example : switches reembedBatch (some 1) = true ∧ switches [.setContract (some 2)] (some 1) = true ∧
    switches reembedBatch none = true ∧ switches seed none = false ∧
    switches [.upsert 1 (some (1, 11)), .upsert 2 (some (1, 21))] (some 1) = false := by decide

/-- **The issue's batch through the fixed mirror.** The flush is durable, the
mirror writes nothing (contract 1's index keeps the build-0 documents,
contract 2's has none), the session is stale and a repair is asked for; the
holder's read falls back to the durable store meanwhile. Not a mirror
failure. -/
def migratedFixed : St := run Pt seeded ([.write reembedBatch] ++ steps .mirStep 4 ++ [.read 2])

/-- Since #319 the mirror holds a contract-switching batch back: the durable
flush lands, the tier goes stale and reads fall back to the durable store. -/
theorem reembed_migration_held_back :
    migratedFixed.d.contract = some 2 ∧ lookup 1 migratedFixed.d.vecs = some (1, 11) ∧
      migratedFixed.tier.sync = .stale ∧ migratedFixed.tier.failures = 0 ∧
      migratedFixed.rep = .head ∧ markerEpoch migratedFixed = some 1 ∧
      fellBack migratedFixed = true ∧
      docAt migratedFixed 1 1 = some (0, 10) ∧ docAt migratedFixed 1 2 = some (0, 20) ∧
      docAt migratedFixed 2 1 = none ∧ docAt migratedFixed 2 2 = none := by decide

/-- Then the repair re-indexes from the durable snapshot: contract 2's index
serves exactly the batch's vectors, the session is in sync at the batch's
epoch, and the session's contract-1 documents are swept. -/
def migratedRepaired : St := run Pt migratedFixed (steps .repStep 20 ++ [.read 2])

/-- Since #319, the repair then re-indexes from the durable snapshot, and the
tier is back in sync under the new contract. -/
theorem reembed_migration_repaired :
    migratedRepaired.tier.sync = .inSync ∧ markerEpoch migratedRepaired = some 2 ∧
      migratedRepaired.rep = .idle ∧
      answerAt migratedRepaired [1, 2, 3] = some [some (1, 11), some (1, 21), none] ∧
      docAt migratedRepaired 1 1 = none ∧ docAt migratedRepaired 1 2 = none := by decide

/-- The relabel through the fixed mirror is repaired too, and contract 2's
index serves the relabelled vectors; so is a switch with `SetEmbedding` first
from an existing contract (the fix repairs it instead of trusting the batch to
rewrite every vector). -/
example :
    let r := run Pt seeded ([.write [.setContract (some 2)]] ++ steps .repStep 20 ++ [.read 2])
    let f := run Pt seeded ([.write [.setContract (some 2), .upsert 1 (some (1, 11)),
      .upsert 2 (some (1, 21))]] ++ steps .repStep 20 ++ [.read 2])
    r.tier.sync = .inSync ∧ answerAt r [1, 2] = some [some (0, 10), some (0, 20)] ∧
      f.tier.sync = .inSync ∧ answerAt f [1, 2] = some [some (1, 11), some (1, 21)] := by decide

/-- `repair_converges` applies to the issue's batch: one repair step after the
flush the task is at its load with every hypothesis met, so the new
contract's index holds exactly the durable vectors, which are the batch's,
for every node. -/
def migratedAtLoad : St := run Pt seeded [.write reembedBatch, .repStep]

example : migratedAtLoad.rep = .load 1 ∧ migratedAtLoad.tier.held = some 1 ∧
    migratedAtLoad.d.lease = 1 ∧ migratedAtLoad.d.row = .ours ∧ migratedAtLoad.d.erased = false ∧
    migratedAtLoad.down = false ∧ checkMarker false migratedAtLoad.mark (some migratedAtLoad.d) = .behind ∧
    nextVersion migratedAtLoad.tier.tok migratedAtLoad.tier.cnt 1 = some (1, 3, shift + 3) ∧
    migratedAtLoad.mark = some (shift + 1, 1) ∧ migratedAtLoad.tier.committed ≤ migratedAtLoad.d.epoch ∧
    migratedAtLoad.tier.again = false ∧ migratedAtLoad.d.vecs = [(2, 1, 21), (1, 1, 11)] ∧
    migratedAtLoad.d.contract = some 2 := by decide

/-- The mirror wrote nothing: the index is still the seed's three documents. -/
theorem migratedAtLoad_rt : migratedAtLoad.rt =
    put (put (put Idx.empty 1 1 (shift + 1) (some (0, 10))) 1 2 (shift + 1) (some (0, 20)))
      1 3 (shift + 1) none := rfl

theorem migratedAtLoad_dom (c n : Nat) : (migratedAtLoad.rt c n).ver < shift + 3 := by
  rw [migratedAtLoad_rt]
  simp [put, Idx.empty, shift]
  split <;> (try split) <;> (try split) <;> decide

example : Matches (run Pt migratedAtLoad (steps .repStep (2 * 2 + 9))) ∧
    (run Pt migratedAtLoad (steps .repStep (2 * 2 + 9))).d.contract = some 2 ∧
    (run Pt migratedAtLoad (steps .repStep (2 * 2 + 9))).d.vecs = [(2, 1, 21), (1, 1, 11)] := by
  obtain ⟨hm, hd, -⟩ := repair_converges Pt migratedAtLoad 1 1 3 (shift + 3) (by decide) (by decide)
    (by decide) (by decide) (by decide) (by decide) (by decide) (by decide)
    (migratedAtLoad_dom)
    (by intro w e h; rw [show migratedAtLoad.mark = some (shift + 1, 1) by decide] at h
        cases h; simp [shift])
    (by decide) (by decide)
  have hl : migratedAtLoad.d.vecs.length = 2 := by decide
  rw [hl] at hm hd
  refine ⟨hm, ?_, ?_⟩ <;> rw [hd] <;> decide

/-- **The #296 review's case: an in-place re-embed briefly mixes two builds.**
`--rewrite-same-contract` drains `UpsertNode`s only; the durable store takes
them in one transaction, but the index takes the bulk item by item (and a
periodic refresh can expose any prefix of it) into the same contract's index,
while the session stays in sync: a read between two items ranks build-1 and
build-0 vectors together. The durable store never holds the mix. -/
def inPlace : List Mut := [.upsert 1 (some (1, 11)), .upsert 2 (some (1, 21))]

/-- #296 review: an in-place re-embed through the tiered store briefly answers
reads from two builds, until the bulk and the marker land. -/
theorem inplace_reembed_mixes_builds :
    let s := run Pt seeded [.write inPlace, .mirStep, .read 1]
    s.tier.sync = .inSync ∧ answerAt s [1, 2] = some [some (1, 11), some (0, 20)] ∧
      s.d.vecs = [(2, 1, 21), (1, 1, 11)] := by decide

/-- Once the bulk and the marker land the answer is one build again. If the
bulk fails part way the holder goes stale and falls back; a reader in another
process does not (`reader_never_rechecks`). -/
example :
    answerAt (run Pt seeded ([.write inPlace] ++ steps .mirStep 4 ++ [.read 1])) [1, 2]
      = some [some (1, 11), some (1, 21)] ∧
    fellBack (run Pt seeded [.write inPlace, .mirStep, .setDown true, .mirStep, .setDown false,
      .read 1]) = true := by decide

/-- **A reader that once found the session in sync never re-checks it.**
`sync_for_read` re-checks only `Unknown` and `Stale` sessions; an `InSync`
one is trusted until this store reloads it or evicts it. Another holder's
mirror fails (its batch deleted node 1), the marker stays behind the durable
epoch, and the reader keeps serving node 1 from the index however much time
passes. -/
def readerSeeded : St := run Pt {} [.takeover, .otherWrite seed true, .read 1]

/-- A tiered reader that once found the session in sync never re-checks the
marker, so it keeps reading the index after another process's write leaves it
behind, until it reloads. -/
theorem reader_never_rechecks :
    let s := run Pt readerSeeded [.otherWrite [.delete 1] false, .tick 1000000, .read 1]
    readerSeeded.tier.sync = .inSync ∧ lookup 1 s.d.vecs = none ∧ markerEpoch s = some 1 ∧
      s.d.epoch = 2 ∧ answerAt s [1] = some [some (0, 10)] := by decide

/-- A reload does see it (`a_reader_that_saw_the_marker_behind_...`). -/
example : fellBack (run Pt readerSeeded [.otherWrite [.delete 1] false, .loadBegin, .loadEnd,
    .read 1]) = true := by decide

/-- **Before #320, a mirror lands after another process's erase.** The mirror
checks only this store's record of the lease, never the durable row
(`ensure_fenced` guards repairs, not mirrors): a flush that committed before
the writer stalled past its lease, then an erase by another process that
swept the index and deleted the marker and reported done, then the stalled
mirror resumes, and the erased session's vector and marker are back in the
index. -/
theorem mirror_lands_after_erase :
    let s := run Pt seeded ([.write [.upsert 4 (some (0, 40))], .eraseOther] ++ steps .mirStep 3)
    s.d.erased = true ∧ docAt s 1 4 = some (0, 40) ∧ markerEpoch s = some 2 ∧
      s.log.take 2 = [.mirror 1, .mirror 1] := by decide

/-- #320 adds a send step to each mirror request. The Boolean says whether
the current bulk or marker has been submitted. Check the lease at submission.
Items from a submitted bulk can still land after erasure. Moving to the
marker starts a new request and requires a fresh check. -/
def fencedMirrorTick (run : St × Bool) : St × Bool :=
  let (s, sent) := run
  let stop := ({ markStale s with mir := .idle }, false)
  match s.mir with
  | .idle => (s, false)
  | .puts _ _ _ [] => (mirStep s, false)
  | .puts t _ _ (_ :: _) =>
    if sent then (mirStep s, true)
    else if isFenced s t then (s, true) else stop
  | .marker t _ _ =>
    if sent then (mirStep s, false)
    else if isFenced s t then (s, true) else stop

/-- An erase before the mirror submits its bulk leaves the erased session's
documents and marker absent. -/
theorem mirror_does_not_submit_after_erase :
    let ready := step Pt seeded (.write [.upsert 4 (some (0, 40))])
    let erased := step Pt ready .eraseOther
    let finished := (fencedMirrorTick (erased, false)).1
    finished.d.erased = true ∧ docAt finished 1 4 = none ∧
      markerEpoch finished = none ∧ finished.mir = .idle := by decide

/-- A bulk already submitted can land after erasure, while the next lease
check prevents submission of the marker for that bulk. -/
theorem mirror_bulk_in_flight_does_not_recreate_marker :
    let ready := step Pt seeded (.write [.upsert 4 (some (0, 40))])
    let sent := fencedMirrorTick (ready, false)
    let erased := (step Pt sent.1 .eraseOther, sent.2)
    let landed := fencedMirrorTick erased
    let markerReady := fencedMirrorTick landed
    let finished := (fencedMirrorTick markerReady).1
    finished.d.erased = true ∧ docAt finished 1 4 = some (0, 40) ∧
      markerEpoch finished = none ∧ finished.mir = .idle := by decide

/-- The repair in the same position writes nothing (#18): its fence reads the
durable row. -/
example :
    let s := run Pt outage ([.write [.upsert 4 (some (0, 40))]] ++ steps .repStep 4 ++
      [.eraseOther] ++ steps .repStep 20)
    s.log.filter (· matches .repair _) = [] ∧ s.mark = none := by decide

/-- **A repair can reconcile from the snapshot it loaded before re-checking an
ahead marker** (`repair_once`). #18 says a marker ahead of a load is re-checked
against a fresh load and never repaired from; but when the fresh re-check
finds the marker *behind* (another flush committed in between), `repair_once`
reconciles from the first, older snapshot. The holder loads while its own e2
mirror is in flight (marker behind: a repair is queued while the session is
still in sync), e2's mirror lands, the repair loads e2, e3 (deleting node 2)
commits and mirrors, e4 commits; the repair's check finds the marker (e3)
ahead of e2, the re-check finds it behind e4, and the pass re-indexes node 2
and rewinds the marker to e2. The overtaken-repair guard (#18) sends it round again, which
repairs it: for the holder, reads fall back meanwhile; another process's
reader in sync (`reader_never_rechecks`) serves the deleted node. -/
def rewound : St :=
  run Pt seeded ([.write [.upsert 4 (some (0, 40))], .loadBegin, .loadEnd] ++ steps .mirStep 3 ++
    [.repStep, .repStep, .write [.delete 2]] ++ steps .mirStep 3 ++
    [.write [.upsert 5 (some (0, 50))]] ++ steps .repStep 14)

/-- A repair pass can reconcile from the snapshot it loaded before re-checking
an advanced marker, so it rewinds the index to that older snapshot. -/
theorem repair_from_pre_recheck_snapshot :
    lookup 2 rewound.d.vecs = none ∧ rewound.d.epoch = 4 ∧
      docAt rewound 1 2 = some (0, 20) ∧ markerEpoch rewound = some 2 ∧
      rewound.tier.sync ≠ .inSync ∧ rewound.rep = .head := by decide

set_option maxRecDepth 8000 in
example :
    let s := run Pt rewound (steps .mirStep 3 ++ steps .repStep 30)
    s.tier.sync = .inSync ∧ docAt s 1 2 = none ∧ docAt s 1 5 = some (0, 50) ∧
      markerEpoch s = some 4 := by decide

/-- **A stale repair pass rewinds the version counter.** `next_version`
restarts the counter whenever the token differs, an older token included,
and `reconcile` calls it before its first fence. A pass that captured token 1
reaches `reconcile` after this store was overridden and re-acquired at token 3
and mirrored node 5 under 3 at counter 1: the pass sets the recorded token
back to 1 and its fence stops it. The next token-3 write starts again at
counter 1, the version node 5's document already has, so its bulk and marker
are refused as conflicts that count as success; the repair it triggers does
the same. The session ends in sync with node 5's old vector served, and the
next mirrored flush advances the marker, so a reload finds it in sync too:
permanent, silent staleness. -/
def stalePass : St :=
  run Pt seeded ([.write [.upsert 4 (some (0, 40))], .loadBegin, .loadEnd, .repStep, .repStep,
    .repStep] ++ steps .mirStep 3 ++ [.takeover, .override, .acquire, .loadBegin, .loadEnd,
    .write [.upsert 5 (some (0, 50))]] ++ steps .mirStep 3)

def stalePassAfter : St :=
  run Pt stalePass ([.repStep, .repStep, .write [.upsert 5 (some (0, 55))]] ++ steps .mirStep 3 ++
    steps .repStep 30 ++ [.read 1])

def stalePassLater : St :=
  run Pt stalePassAfter ([.write [.upsert 6 (some (0, 60))]] ++ steps .mirStep 3 ++
    [.loadBegin, .loadEnd] ++ steps .repStep 30 ++ [.read 1])

set_option maxRecDepth 20000 in
/-- `stalePass`'s run, pinned: the stale pass rewinds the recorded token, the
next write's version repeats one already in the index, and the old vector is
served while the session is in sync, through a later flush and a reload. -/
theorem stale_pass_rewinds_version :
    stalePass.tier.tok = 3 ∧ stalePass.tier.cnt = 1 ∧ (stalePass.rt 1 5).ver = 3 * shift + 1 ∧
      (run Pt stalePass [.repStep]).tier.tok = 1 ∧ (run Pt stalePass [.repStep]).tier.cnt = 1 ∧
      stalePassAfter.tier.sync = .inSync ∧ lookup 5 stalePassAfter.d.vecs = some (0, 55) ∧
      answerAt stalePassAfter [5] = some [some (0, 50)] ∧
      stalePassLater.tier.sync = .inSync ∧ markerEpoch stalePassLater = some 5 ∧
      stalePassLater.d.epoch = 5 ∧ lookup 5 stalePassLater.d.vecs = some (0, 55) ∧
      answerAt stalePassLater [5, 6] = some [some (0, 50), some (0, 60)] ∧
      stalePassLater.rep = .idle := by decide

/-- **A batch that deletes only edges leaves the marker behind.** It names no
session and deletes no node, so `mirror_unnamed` returns before attributing
it: the durable epoch moves and the marker does not. The holder stays in sync
(the index content is right), but its next load finds the marker behind and
re-indexes the whole session, and a reader falls back until the holder's next
mirrored flush. -/
theorem edge_only_batch_leaves_marker_behind :
    let s := run Pt seeded [.write [.deleteEdge]]
    s.d.epoch = 2 ∧ markerEpoch s = some 1 ∧ s.tier.sync = .inSync ∧
      (run Pt s [.loadBegin, .loadEnd]).rep = .head := by decide

end Runs

end Lambo.Tiered
