/-
# Write-queue drain: worked examples and findings

Concrete cases, checked by `decide`, over every layer of the drain model,
the history layer included: what the model shows about the Rust, and the
fixes on the issues' scenarios.

**Every theorem in this file is a finding about the Rust before a fix**
(#294, #295, #345); the current behaviour on the same scenarios is in its
`example`s. So a docs page may not cite this file: the anchor check
(`scripts/docs/check-doc-anchors.py`) refuses it, as it refuses
`History.lean`, and a page cites the propositions of the specification
(`Drain/Spec/`) instead.

## Results

`transient_skip_reorders` and `outage_then_replay_reorders` (a transient
embed answer breaks the per-agent order across serves; both halves fixed by
#295: the replay defers the agent, the worker holds its later writes),
`clock_step_reorders` (the load order trusts the wall clock; fixed by #295),
`outage_failed_then_applied` (before #295, a doc fix: `ReceiptAnswer::Failed`
said the write did not happen, and after an outage it later did; since #295
an outage answers `pending_replay`, not `failed`),
`store_error_blocks_backlog` (a graph-dependent refusal raised as a plain
`Invariant` stops the replay on every serve; fixed by #294),
`bad_id_never_settled` (fixed by #295), `closeW_vouches_dead_lettered` (the
close before #345 vouches for a dead-lettered put; fixed by #345).

Boundary: each case is one concrete run over the earlier layers; the general
statements are in the other drain files.
-/

import LamboProofs.Drain.Proofs.Close
import LamboProofs.Drain.History

namespace Lambo.Drain

open Lambo.Receipts (Answer Entry settle Lane)

/-! ## What the model shows about the Rust: concrete cases -/


private def j1 : Job := ⟨1, 0, 1000, false, 0⟩
private def j2 : Job := ⟨2, 0, 1001, false, 0⟩
private def j3 : Job := ⟨3, 0, 1002, false, 0⟩

private def okAll : Job → RR := fun _ => .run .ok

/-- Drain to completion takes a lane's jobs in submission order, whatever the
interleaving (`drain_fifo`). -/
example : (drain (Lambo.Receipts.run ⟨[], []⟩ [.push j1, .pop, .push j2, .push j3])).taken =
    [j1, j2, j3] := by decide

/-! The next four examples are over the worker **before #295** (`work`,
`close`, `History.lean`); the current worker's are under "The close over the
current worker" below. -/

/-- `drain_settles_each` (before #295): one job applied, one refused for its
content, one hit by an outage; all three settled. The current worker holds the
third (`pending_replay`) instead. -/
example :
    let w := work (fun _ => false)
      (fun j => if j.rid = 1 then .ok else if j.rid = 2 then .err .embed else .err .unavailable)
      0 (admitStore [j1, j2, j3]) [j1, j2, j3]
    ([j1.key, j2.key, j3.key].map (fun r => (lookup w.1 r).map (·.answer)) =
      [some .applied, some .failed, some .failed]) ∧
    w.2 = [[.write j1.key, .consume j1.key .applied], [.consume j2.key .failed], []] := by decide

/-- `shutdown_no_silent_loss` (before #295): the drain finished `j1`; `j2` and
`j3` are deferred and owed by the next replay. -/
example :
    let c := close [j1] (fun _ => false) (fun _ => .ok) 0 .durable (admitStore [j1, j2, j3])
    [j1.key, j2.key, j3.key].map (fun r => (lookup c.1 r).map (·.answer)) =
        [some .applied, some .intentRecorded, some .intentRecorded] ∧
      (pendingOf (loadOrder (intentsOf (admitLog [j1, j2, j3] ++ c.2).flatten))).map (·.job) =
        [j2, j3] := by decide

/-- `close_answer` (before #295), incomplete flush: the deferred write says
`close_unflushed`, and a retried close whose flush lands upgrades it
(`retry_upgrades`). -/
example :
    let c := close [] (fun _ => false) (fun _ => .ok) 0 .incomplete (admitStore [j1])
    (lookup c.1 j1.key).map (·.answer) = some .closeUnflushed ∧
      (lookup (settleDeferred .durable 1 [j1.key] c.1) j1.key).map (·.answer) = some .intentRecorded := by
  decide

/-- `drop_settles_nothing`: the `Drop` path leaves a pending receipt pending. -/
example : (lookup (abortAllSync (admitStore [j1])) j1.key).map (·.answer) = some .pending := by decide

/-- `crash_never_reapplies` (before #295): the process dies after flushing
`j1`'s commit; the next replay applies only `j2`. -/
example :
    let log := admitLog [j1, j2] ++
      (work (fun _ => false) (fun _ => .ok) 0 (admitStore [j1, j2]) [j1, j2]).2
    (replay true okAll (loadOrder (intentsOf (log.take 3).flatten))).applied = [j2] := by decide

/-- `replay_drained`, `replay_idempotent`: a second replay over what the
first left does nothing. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, some .applied⟩, ⟨j3, none⟩]
    let r1 := replay true okAll ints
    r1.applied = [j1, j3] ∧ replay true okAll (applyLog ints r1.log) = ROut.empty := by decide

/-- Two crashed processes' backlogs with colliding sequence numbers: the
rows are distinct receipts (different epochs and issue times). `a` applies
while `b` meets a transient embed answer and is skipped; `a`'s consume does
not consume `b`'s row, so the next replay applies `b`. Keyed by sequence
alone, `a`'s consume would mark `b`'s row too and the next replay would apply
nothing: the dropped write this keying rules out. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 7⟩
    let b : Job := ⟨1, 0, 2000, false, 9⟩
    let ints := loadOrder [⟨b, none⟩, ⟨a, none⟩]
    let rout : Job → RR := fun j => if j.epoch = 9 then .run (.err .unavailable) else .run .ok
    let r := replay true rout ints
    r.applied = [a] ∧ (replay true okAll (applyLog ints r.log)).applied = [b] := by decide

/-- Both processes' rows applied in one serve, in issue-time order. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 7⟩
    let b : Job := ⟨1, 0, 2000, false, 9⟩
    let ints := loadOrder [⟨b, none⟩, ⟨a, none⟩]
    let r := replay true okAll ints
    r.applied = [a, b] ∧ replay true okAll (applyLog ints r.log) = ROut.empty := by decide

/-- `inputFact_goes_on`: a cycle refusal settles `failed` and the replay
applies the next intent. -/
example :
    let r := replay true (fun j => if j.rid = 1 then .run (.err .cycle) else .run .ok)
      [⟨j1, none⟩, ⟨j2, none⟩]
    r.failed = [j1] ∧ r.applied = [j2] ∧ r.block = none := by decide

/-- The liveness gate: a dead embedder at attach consumes nothing. -/
example : replay false okAll [⟨j1, none⟩] = { ROut.empty with block := some .embedder } := by
  decide

/-- `REPLAY_LOOKUP_TIMEOUT_LIMIT`: three lookup timeouts in a row settle two
`failed` and stop at the third, which stays owed. -/
example :
    let r := replay true (fun _ => .run (.err .lookupTimeout))
      [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    r.failed = [j1, j2] ∧ r.block = some .other := by decide

/-- `EMBEDDER_SICK_THRESHOLD`: three transients in a row stop the replay as
`embedder` and consume nothing. -/
example :
    let r := replay true (fun _ => .run (.err .unavailable))
      [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    r.log = [] ∧ r.block = some .embedder := by decide

/-! ### Finding 1: a transient skip breaks the per-agent replay order

On an `EmbedUnavailable` below the threshold the loop `continue`s
(`replay.rs`, the `Err(LamboError::EmbedUnavailable(e))` arm), leaving the
intent unconsumed and applying the next one, which can be the same agent's
later write. The skipped intent is applied by a later serve, after it. The
`WriteIntent::agent` doc says "Replay preserves order **within** an agent's
intents (the lane promise)"; `docs/reference/mcp.mdx` (the `intent_durable`
row) says the next server re-attempts the write "in the order you sent it";
and the image-`Config` arm stops rather than skip because "skipping it would
break the lane order".

The same reordering happens across a restart through the in-session outage
path: `j1` meets `EmbedUnavailable` in session (answered `failed`, intent
left unconsumed), `j2` applies in session, and `j1` applies at the next serve
(`outage_then_replay_reorders`). That contradicts `docs/reference/mcp.mdx`
line 91: "sequential writes from one agent to the same session apply in
submission order". -/

/-- Two writes of agent 0; at the first serve `j1` meets one transient embed
answer and `j2` applies; the next serve applies `j1`. Applied order across
the two serves: `j2`, then `j1`. -/
theorem transient_skip_reorders :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let r1 := replay true (fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok) ints
    let r2 := replay true okAll (applyLog ints r1.log)
    j1.agent = j2.agent ∧ j1.rid < j2.rid ∧ r1.applied ++ r2.applied = [j2, j1] := by
  decide

/-- Two sequential writes of one agent (`j1` before `j2`). In session the
worker meets an outage at `j1` and applies `j2`; the close lands; the next
serve applies `j1`. Writes in the order they reached the graph: `j2`, then
`j1`. Over the worker **before #295** (`close`); the current worker holds `j2`
behind `j1` (`stepW_ord`, `sessionW_then_serve_order`, `outage_holdsW`). -/
theorem outage_then_replay_reorders :
    let c := close [j1, j2] (fun _ => false)
      (fun j => if j.rid = 1 then .err .unavailable else .ok) 0 .durable (admitStore [j1, j2])
    let rows := loadOrder (intentsOf (admitLog [j1, j2] ++ c.2).flatten)
    j1.agent = j2.agent ∧ j1.rid < j2.rid ∧
    c.2 = [[], [.write j2.key, .consume j2.key .applied]] ∧
      (replay true okAll rows).applied = [j1] := by
  decide

/-! ### Finding 2: the load order trusts the wall clock

Rows load `ORDER BY issued_ms, lane_seq`, and `issued_ms` is the pipeline's
clock (`Utc::now` in production) read in `next_receipt`. The
`WriteIntent::lane_seq` doc says sorting by (`issued_ms`, `lane_seq`)
"replays one process's intents in exact admission order"; that holds only
while the clock does not step back between admissions (`load_seq_order`). -/

/-- One process, one agent: `j1` admitted first (sequence 1) at 1000 ms, then
the clock steps back one millisecond and `j2` (sequence 2) is stamped 999 ms.
The load puts `j2` first, and the replay applies it first. -/
theorem clock_step_reorders :
    let a : Job := ⟨1, 0, 1000, false, 0⟩
    let b : Job := ⟨2, 0, 999, false, 0⟩
    (replay true okAll (loadOrder [⟨a, none⟩, ⟨b, none⟩])).applied = [b, a] := by
  decide

/-! ### Finding 3 (doc fix): a `failed` receipt after an outage is not final

For an in-session `EmbedUnavailable` the worker answers `failed` but leaves
the intent unconsumed (`outage_failed_but_owed`), so the next serve applies
it and the same receipt id then answers `applied_after_restart`. This is the
designed behaviour, and the user docs describe it: `docs/reference/mcp.mdx`
and `docs/reference/config.mdx` tell callers to recall, or read the receipt
after the restart, before deriving the same concept again, "or it may be
written twice", and the `failed` row of mcp.mdx's receipt table says the
intent is kept and replayed. What is stale is the rustdoc on
`ReceiptAnswer::Failed` (`src/writeq/receipts.rs`), "Attempted and rejected.
The write did not happen.", with no qualifier, and the first sentence of
mcp.mdx's `failed` row, "Nothing was written", which needs "by this server"
or "yet".

**This finding is about the worker before #295.** #295 removed its cause:
the current worker no longer answers `failed` for an outage but holds the
write (`pending_replay`, `outage_holdsW`), and the close answers a held write
`intent_durable` (`held_at_closeW`), so the receipt goes from
`pending_replay` or `intent_durable` to `applied_after_restart` with no
`failed` between. The `ReceiptAnswer::Failed` rustdoc and mcp.mdx's `failed`
row now say an outage does not answer `failed`. -/

/-- The restart map's answer for a receipt after a replay: seeded from the
rows, then overwritten by the replay's own inserts. -/
def restartAnswer (live : Bool) (rout : Job → RR) (intents : List Intent) (r : Key) :
    Option Answer :=
  lookupLast (seed intents ++ (replay live rout intents).answers) r

/-- **Before #295: a `failed` receipt for an outage's write later answered
`applied_after_restart`.** Under the worker before #295 (`close`, `workerStep`)
a write met by an in-session outage answers `failed`, and the next serve
applies it. **The current Rust does not behave this way**: since #295 that
write answers `pending_replay` in session and `intent_durable` at close, never
`failed` (`outage_holdsW`, `held_at_closeW`; the example "`outage_holdsW`
beside `outage_failed_then_applied`" below runs both on
this scenario). Kept because it documents the receipt #295's hold replaced,
the finding above. -/
theorem outage_failed_then_applied :
    let c := close [j1] (fun _ => false) (fun _ => .err .unavailable) 0 .durable (admitStore [j1])
    let rows := loadOrder (intentsOf (admitLog [j1] ++ c.2).flatten)
    (lookup c.1 j1.key).map (·.answer) = some .failed ∧
      restartAnswer true okAll rows j1.key = some .appliedAfterRestart ∧
      (replay true okAll rows).applied = [j1] := by
  decide

/-! ### Finding 4: a store error other than a cycle blocks the backlog

Only a cycle refusal among `StoreError::Invariant`s settles `failed` at
replay; every other one reaches the stop arm, which leaves the intent durable
and ends the replay, on every serve, so every later intent stays
`pending_replay` (the shape #116 fixed for cycles). Some of those are facts
about the write's input against the graph, raised as plain `Invariant`s:
`reject_repeated_observation` and `reject_second_hierarchical_parent`
(`src/graph/derive.rs`) and the same-canonical-key check in hybrid
`validate_pairs` (`src/graph/hybrid.rs`). Scenario: a `Canonical` session
acks two derives of the same Observation, both puts flush, the process dies;
the next serve applies the first, the second is refused with a non-cycle
`Invariant`, and the replay stops (block reason `Other`) there on every later
serve, leaving the whole backlog behind it `pending_replay`. The in-session
worker, meeting the same error, consumes it `failed` (the theorem states it of
`workerStep`, the worker before #295; the current worker too, since `finishW`
settles a store error `failed` with the same `[consume failed]` batch).
(`spawn_replay`'s doc
also lists "a vanished interaction" among the failures consumed `failed`; that
is a `Store(NotFound)` and stops the replay too, but it is effectively
unreachable: the interaction is logged before the put and never collected.) -/

/-- Before #294, a graph-dependent refusal raised as a plain `Invariant`
stopped `spawn_replay` on every serve, so the backlog behind it never drained.
-/
theorem store_error_blocks_backlog :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .store) else .run .ok
    let r := replay true rout ints
    r.applied = [] ∧ r.block = some .other ∧ applyLog ints r.log = ints ∧
      workerStep false j1 (.err .store) = (.failed, [.consume j1.key .failed]) := by
  decide

/-- #294 fixed, on the issue's scenario: a `Canonical` session's second
derive of one Observation (`j2`) is refused at replay as an input refusal. The
old replay stops there, `j3` unapplied, on every serve; the fixed one settles
`j2` `failed`, applies `j3`, and leaves nothing for the next serve. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    let rout : Job → RR := fun j => if j.rid = 2 then .run (.err .inputRefusal) else .run .ok
    let old := replay true rout ints
    let r := replayWith stepInput true rout ints
    old.applied = [j1] ∧ old.block = some .other ∧
      replay true rout (applyLog ints old.log) = { ROut.empty with block := some .other } ∧
      r.applied = [j1, j3] ∧ r.failed = [j2] ∧ r.block = none ∧
      replayWith stepInput true rout (applyLog ints r.log) = ROut.empty := by
  decide

/-- The fixed replay still stops on a store error that is not an input
refusal (the scenario of `store_error_blocks_backlog`). -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .store) else .run .ok
    replayWith stepInput true rout ints = replay true rout ints ∧
      (replayWith stepInput true rout ints).block = some .other := by
  decide

/-! ### Finding 5: an intent with an unparseable receipt id is never settled

The loop `continue`s past it (`let Ok(receipt) = ... else { continue; }`),
so it is never applied and never consumed, and every serve skips it again.
Seeding's warning says "it will replay but cannot be looked up", which is
false: it never replays. And the row stays counted in `replay_owed` (set from
the backlog's length, decremented only when a row is applied or failed)
forever. -/

/-- Before #295, `spawn_replay` skipped an intent whose receipt id did not
parse and never settled it. -/
theorem bad_id_never_settled :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .badId else .run .ok
    let r := replay true rout ints
    r.applied = [j2] ∧ ⟨j1, none⟩ ∈ pendingOf (applyLog ints r.log) := by
  decide

/-! ### The fixes for #295 on the issue's scenarios -/

private def k1 : Job := ⟨3, 1, 1002, false, 0⟩

/-- `transient_skip_reorders` fixed: `j1` meets one transient answer, so
agent 0 is deferred and its `j2` waits; agent 1's `k1` applies. The next
serve applies `j1`, then `j2`. The old replay applied `j2` first. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok
    let r1 := replayD stepFixed true rout ints
    let r2 := replayD stepFixed true okAll (applyLog ints r1.log)
    (replay true rout ints).applied = [j2, k1] ∧
      r1.applied = [k1] ∧ r1.block = none ∧ r2.applied = [j1, j2] ∧
      r1.applied ++ r2.applied = [k1, j1, j2] := by
  decide

/-- The threshold still counts transients across agents: three agents each
meeting one, with nothing answered between, stop the replay as `embedder`,
consuming nothing. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 0⟩
    let b : Job := ⟨2, 1, 1001, false, 0⟩
    let c : Job := ⟨3, 2, 1002, false, 0⟩
    let r := replayD stepFixed true (fun _ => .run (.err .unavailable))
      [⟨a, none⟩, ⟨b, none⟩, ⟨c, none⟩]
    r.log = [] ∧ r.block = some .embedder := by
  decide

/-- `clock_step_reorders` fixed: the clock stepped back between `a` and `b`
of one process; the fixed load puts `a` (sequence 1) first anyway, and the
replay applies `a`, then `b`. -/
example :
    let a : Job := ⟨1, 0, 1000, false, 0⟩
    let b : Job := ⟨2, 0, 999, false, 0⟩
    fixedLoad [⟨a, none⟩, ⟨b, none⟩] = [⟨a, none⟩, ⟨b, none⟩] ∧
      (replayD stepFixed true okAll (fixedLoad [⟨a, none⟩, ⟨b, none⟩])).applied = [a, b] := by
  decide

/-- Across processes the order is the old one: two crashed processes whose
clocks interleave keep their positions, and within process 7 the clock step
is undone. -/
example :
    let a1 : Job := ⟨1, 0, 1000, false, 7⟩
    let a2 : Job := ⟨2, 0, 990, false, 7⟩
    let b1 : Job := ⟨1, 1, 995, false, 9⟩
    let rows : List Intent := [⟨a1, none⟩, ⟨a2, none⟩, ⟨b1, none⟩]
    (loadOrder rows).map (·.job) = [a2, b1, a1] ∧
      (fixedLoad rows).map (·.job) = [a1, b1, a2] := by
  decide

/-- `bad_id_never_settled` fixed: the row with the unparseable id is settled
`failed`, and nothing is left owed. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .badId else .run .ok
    let r := replayD stepFixed true rout ints
    r.applied = [j2] ∧ r.failed = [j1] ∧ pendingOf (applyLog ints r.log) = [] := by
  decide

/-- #294's scenario through the fully fixed replay. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨j3, none⟩]
    let rout : Job → RR := fun j => if j.rid = 2 then .run (.err .inputRefusal) else .run .ok
    let r := replayD stepFixed true rout ints
    r.applied = [j1, j3] ∧ r.failed = [j2] ∧ r.block = none := by
  decide

/-! ### The in-session hold on the issue's scenario -/

/-- `outage_then_replay_reorders` fixed. `j1` meets an outage; when `j2`
arrives the embedder is still down, so `j1` is retried, fails again, and
`j2` is held behind it, unrun. Both receipts say `pending_replay`; agent 1's
`k1` applies meanwhile. The next serve applies `j1`, then `j2`. -/
example :
    let out : Nat → Job → RunResult := fun _ j =>
      if j.agent = 0 then .err .unavailable else .ok
    let st := workH out 0 0 (HState.init (admitStore [j1, j2, k1])) [j1, j2, k1]
    let rows : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, some .applied⟩]
    st.applied = [k1] ∧ st.held = [j1, j2] ∧
      [j1.key, j2.key, k1.key].map (fun r => (lookup st.store r).map (·.answer)) =
        [some .pendingReplay, some .pendingReplay, some .applied] ∧
      (replayD stepFixed true okAll (fixedLoad rows)).applied = [j1, j2] := by
  decide

/-- The embedder is back when `j2` arrives: the held `j1` applies first, then
`j2`, in this serve. -/
example :
    let out : Nat → Job → RunResult := fun t _ => if t = 0 then .err .unavailable else .ok
    let st := workH out 0 0 (HState.init (admitStore [j1, j2])) [j1, j2]
    st.applied = [j1, j2] ∧ st.held = [] ∧
      [j1.key, j2.key].map (fun r => (lookup st.store r).map (·.answer)) =
        [some .applied, some .applied] := by
  decide

/-! ### The retry pass, quarantine and the block reason -/

/-- A write that blinked once no longer waits for the next serve: the retry
pass applies `j1`, then `j2`, after `k1`; nothing is owed and nothing is
blocked. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok
    let r := serveQ true rout okAll ⟨1, 0⟩ ints
    r.applied = [k1, j1, j2] ∧ r.block = none ∧ pendingOf (applyLog ints r.log) = [] := by
  decide

/-- A write that never embeds keeps its agent's writes owed, and the serve
says why (`embedder`). -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .run (.err .unavailable) else .run .ok
    let r := serveQ true rout rout ⟨1, 0⟩ ints
    r.applied = [k1] ∧ r.block = some .embedder ∧
      (pendingOf (applyLog ints r.log)).map (·.job) = [j1, j2] := by
  decide

/-- An id in another format is quarantined, not settled: it and its agent's
later write stay owed, `k1` applies, and the serve says `other`. The fix as
first committed settled it `failed`. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩, ⟨k1, none⟩]
    let rout : Job → RR := fun j => if j.rid = 1 then .unknownFormat else .run .ok
    let r := serveQ true rout rout ⟨0, 0⟩ ints
    (replayD stepFixed true rout ints).failed = [j1] ∧
      r.applied = [k1] ∧ r.failed = [] ∧ r.block = some .other ∧
      (pendingOf (applyLog ints r.log)).map (·.job) = [j1, j2] := by
  decide

/-! ### The hold with admission, caps, retries and the fence -/

private def capsT : Caps := ⟨2, 4, 100⟩
private def one : Job → Nat := fun _ => 1

/-- The lane wedge the review found, gone. `j1` meets an outage and is held;
`j2` is admitted and joins it; `j3` is refused, the lane of two being full of
held writes, and the refusal wakes the retry. With no further write, the
retry applies `j1`, then `j2`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2, j3]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0, .submit j3]
    let st2 := runW capsT one 0 st [.wake 0, .start 0, .finish 0 .ok, .start 0, .finish 0 .ok]
    st.held = [j1, j2] ∧ st.admitted = [j1, j2] ∧ st2.applied = [j1, j2] ∧ st2.held = [] := by
  decide

/-- Held bytes count: with a 3-byte cap and 2-byte writes, the held `j1`
leaves no room for `j2`. -/
example :
    let st := runW ⟨8, 8, 3⟩ (fun _ => 2) 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2]
    st.held = [j1] ∧ st.admitted = [j1] := by
  decide

/-- The lease is lost in the middle of a retry: `j1`, already running,
commits (the store refuses that flush); the held `j2` is refused without
being run. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0,
       .wake 0, .start 0, .fence, .finish 0 .ok, .start 0]
    st.applied = [j1] ∧ st.settled.map (·.1) = [j1, j2] ∧ st.held = [] := by
  decide

/-- A lookup timeout settles `failed` and holds nothing: the agent's next
write applies. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .lookupTimeout), .submit j2, .start 0, .finish 0 .ok]
    st.held = [] ∧ st.settled = [(j1, .failed), (j2, .applied)] := by
  decide

/-- The halt the review found hidden: a replay halted on a lost lease leaves
`j2` owed and reports `null`; `serveR` reports `other`. -/
example :
    let ints : List Intent := [⟨j1, none⟩, ⟨j2, none⟩]
    let rout : Job → RR := fun j => if j.rid = 2 then .halt else .run .ok
    reported (serveQ true rout rout ⟨0, 0⟩ ints).block = none ∧
      pendingOf (applyLog ints (serveQ true rout rout ⟨0, 0⟩ ints).log) ≠ [] ∧
      (serveR true rout rout ⟨0, 0⟩ ints).block = some .other := by
  decide

/-! ### The close over the current worker -/

/-- `held_at_closeW`, `shutdown_no_silent_lossW` on #295's scenario. `j1`
meets an outage and is held; `j2` joins it, unrun; agent 1's `k1` applies.
Both held writes answer `pending_replay` while held; the close never answers
them `failed`, but `intent_durable` after a completed flush and `close_unflushed`
after an incomplete one, leaves `k1` `applied`, and the flushed log owes
exactly `j1` and `j2`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2, k1]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0,
       .submit k1, .start 1, .finish 1 .ok]
    st.held = [j1, j2] ∧ st.applied = [k1] ∧
      [j1.key, j2.key, k1.key].map (ansOf st.store) =
        [some .pendingReplay, some .pendingReplay, some .applied] ∧
      [j1.key, j2.key, k1.key].map (ansOf (closeW .durable 1 st)) =
        [some .intentRecorded, some .intentRecorded, some .applied] ∧
      [j1.key, j2.key, k1.key].map (ansOf (closeW .incomplete 1 st)) =
        [some .closeUnflushed, some .closeUnflushed, some .applied] ∧
      (pendingOf (fixedLoad (intentsOf (admitLog [j1, j2, k1] ++ st.log).flatten))).map (·.job) =
        [j1, j2] := by
  decide

/-- `outage_holdsW` beside `outage_failed_then_applied`: the same write under
the current worker answers `intent_durable` at close (and the next serve
applies it), where the worker before #295 answered it `failed`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable)]
    let rows := fixedLoad (intentsOf (admitLog [j1] ++ st.log).flatten)
    ansOf (closeW .durable 1 st) j1.key = some .intentRecorded ∧
      (serveR true okAll okAll ⟨0, 0⟩ rows).applied = [j1] ∧
      (lookup (close [j1] (fun _ => false) (fun _ => .err .unavailable) 0 .durable
        (admitStore [j1])).1 j1.key).map (·.answer) = some .failed := by
  decide

/-- The fence at close: `j1`, already running, commits (its batch is in the
log; the store refuses that flush); `j2` is refused `failed` unrun and its
intent is left unconsumed (the session's current holder's to apply, if an
earlier flush took it). -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .submit j2, .start 0, .fence, .finish 0 .ok, .start 0]
    [j1.key, j2.key].map (ansOf (closeW .incomplete 1 st)) = [some .applied, some .failed] ∧
      st.log = [[.write j1.key, .consume j1.key .applied]] ∧
      (pendingOf (fixedLoad (intentsOf (admitLog [j1, j2] ++ st.log).flatten))).map (·.job) =
        [j2] := by
  decide

/-- A write running when the close starts (aborted before its commit) and one
still queued: both `close_unflushed` after an incomplete flush, and both
`intent_durable` once a retried close's flush lands (`closeW_retry_upgradesW`). -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .submit j2, .start 0]
    st.running = [j1] ∧ st.queue = [j2] ∧
      [j1.key, j2.key].map (ansOf (closeW .incomplete 1 st)) =
        [some .closeUnflushed, some .closeUnflushed] ∧
      [j1.key, j2.key].map
        (ansOf (settleDeferred .durable 2 (deferredW st) (closeW .incomplete 1 st))) =
        [some .intentRecorded, some .intentRecorded] := by
  decide

/-- `crash_never_reappliesW`: the process dies after flushing `j1`'s commit;
the next serve applies only `j2`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .submit j2, .start 0, .finish 0 .ok, .start 0, .finish 0 .ok]
    let log := admitLog [j1, j2] ++ st.log
    (serveR true okAll okAll ⟨0, 0⟩ (fixedLoad (intentsOf (log.take 3).flatten))).applied =
      [j2] := by
  decide

/-! ### Dead-lettered batches at close (#345)

The concrete cases of `Close.lean`'s section "Dead-lettered batches at close
(#345)". -/

/-- **#345, the concrete case: the current close vouches for a dead-lettered
put.** `j1` meets an embedder outage and is held (`pending_replay`); the
background flush that drained its put dead-letters the batch, so every batch
of the log has landed and the close's tail is empty. The current close
(`closeW .durable`, the empty-tail branch's `CloseFlush::Durable`) answers it
`intent_durable`, though its put is not committed; the fixed close answers it
`close_unflushed`. Rust test
`a_close_with_an_empty_tail_does_not_vouch_for_a_dead_lettered_intent`. -/
theorem closeW_vouches_dead_lettered :
    let st := runW capsT one 0 (WS.init (admitStore [j1]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable)]
    let log := sessLog [j1] st
    let hist := [Landed.deadLettered]
    hist.length = log.length ∧ ansOf st.store j1.key = some .pendingReplay ∧
      ansOf (closeW .durable 1 st) j1.key = some .intentRecorded ∧
      j1.key ∉ committedPuts log hist ∧
      ansOf (closeD .durable (droppedPuts log hist) 1 st) j1.key = some .closeUnflushed := by
  decide

/-- The retried close on the second Rust test's scenario: `j1`'s put is
dead-lettered, then `j2` is held behind it and its put rides the tail. The
first close's final flush fails (`close_unflushed` for both); the retry's
lands the tail, so `j2` is upgraded to `intent_durable` and `j1` stays
`close_unflushed`, where the current close upgraded both. Rust test
`a_retried_close_upgrades_only_the_intents_it_can_vouch_for`. -/
example :
    let st := runW capsT one 0 (WS.init (admitStore [j1, j2]))
      [.submit j1, .start 0, .finish 0 (.err .unavailable), .submit j2, .start 0]
    let log := sessLog [j1, j2] st
    let hist := [Landed.deadLettered, Landed.committed]
    let d := droppedPuts log hist
    st.held = [j1, j2] ∧ hist.length = log.length ∧
      [j1.key, j2.key].map (ansOf (closeD .incomplete d 1 st)) =
        [some .closeUnflushed, some .closeUnflushed] ∧
      [j1.key, j2.key].map
        (ansOf (settleDeferredD .durable d 2 (deferredW st) (closeD .incomplete d 1 st))) =
        [some .closeUnflushed, some .intentRecorded] ∧
      [j1.key, j2.key].map
        (ansOf (settleDeferred .durable 2 (deferredW st) (closeW .incomplete 1 st))) =
        [some .intentRecorded, some .intentRecorded] := by
  decide

end Lambo.Drain
