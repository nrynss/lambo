/-
# Oracle vectors for `Canon.lean`

The pure parts of the canonization model:

* `Canon.gate current from to` (and so `Canon.legal`), exhaustively, against
  `Graph::apply_canonization_transition` on a concept whose status is
  `current`, with an event `from → to` and no demotion reason (the reason
  check is not modelled): `Ok` and the concept's new status is `some to`, an
  error is `none`;
* `Canon.stage1Passes` against `stage1_evidence_passes`;
* `Canon.stage2Passes distinct num den` against `stage2_evidence_passes` with
  coverage `num / den` (`den ≤ 1000`, so the correctly rounded quotient is on
  the same side of `0.3` as the fraction);
* `Canon.inCooldown` against `in_repromotion_cooldown`, times in
  milliseconds since the epoch, small enough that the Rust's overflow
  branches (which the model leaves out) are never taken.

`Canon.cycle` is not covered: its flags stand for "this stage would promote
the concept", and building a `CyclePlan`, `Verdicts` and `EvalParams` that
mean exactly that (with the supersession re-checks, the Solo/Swarm policy
seam and the event channel) needs fixtures far larger than the model.
-/
import LamboProofs.Canon
import OracleExport.Gen

namespace Lambo.Oracle.Canon

open Lean Lambo.Canon Lambo.Oracle

def statuses : List Status := [.none, .candidate, .venerable, .canonical]

def statusName : Status → String
  | .none => "none"
  | .candidate => "candidate"
  | .venerable => "venerable"
  | .canonical => "canonical"

def gateCases : List Json :=
  statuses.flatMap fun c => statuses.flatMap fun f => statuses.map fun t =>
    case (toJson #[statusName c, statusName f, statusName t]) (toJson ((gate c f t).map statusName))

def genStage1 : Gen (Nat × Nat) := do
  return (← pick [0, 1, 2, 3, 4, 10], ← pick [0, 1, 2, 3, 7])

def genStage2 : Gen (Nat × Nat × Nat) := do
  let distinct ← pick [0, 1, 2, 3, 4, 9]
  let den ← pick [1, 3, 7, 10, 20, 100, 997, 1000]
  -- Around 0.3 · den, and the ends.
  let near := (3 * den) / 10
  let num ← pick [0, den, near, near + 1, near - 1, (← range 0 den)]
  return (distinct, min num den, den)

def genCooldown : Gen (Option Int × Int × Int) := do
  let t : Int := 1752000000000
  let cooldown : Int ← pick [0, 1, 1000, 86400000]
  let last ← if ← chance 1 5 then pure none else pure (some t)
  let delta : Int ← pick [-1, 0, 1, cooldown - 1, cooldown, cooldown + 1, 5000]
  return (last, cooldown, t + delta)

def seed : UInt64 := 0x43414E4F4E4C -- "CANONL"

def vectors : Vectors := run seed do
  let s1 ← many 40 genStage1
  let s2 ← many 120 genStage2
  let cd ← many 60 genCooldown
  return {
    model := "Canon", seed
    notes := [
      "gate: [current, from, to] statuses, exhaustively; output the new status, or null for a refusal. The Rust event carries no demotion reason.",
      "stage2Passes: [distinct, num, den], coverage num / den with den <= 1000.",
      "inCooldown: [last_demotion_ms or null, cooldown_ms, now_ms], milliseconds since the epoch.",
      "Canon.cycle is not covered: its Rust input (CyclePlan, Verdicts, EvalParams) cannot be built from flags without large fixtures."
    ]
    fns := [
      { lean := "Lambo.Canon.gate", rust := "graph::Graph::apply_canonization_transition", cases := gateCases },
      { lean := "Lambo.Canon.stage1Passes", rust := "canon::stage1::stage1_evidence_passes",
        cases := s1.map fun (g, d) => case (toJson #[g, d]) (toJson (stage1Passes g d)) },
      { lean := "Lambo.Canon.stage2Passes", rust := "canon::stage2_evidence_passes",
        cases := s2.map fun (d, n, m) => case (toJson #[d, n, m]) (toJson (stage2Passes d n m)) },
      { lean := "Lambo.Canon.inCooldown", rust := "canon::stage3::in_repromotion_cooldown",
        cases := cd.map fun (l, c, n) =>
          case (Json.arr #[toJson l, toJson c, toJson n]) (toJson (inCooldown l c n)) }
    ]
  }

end Lambo.Oracle.Canon
