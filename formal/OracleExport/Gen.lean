/-
# The differential oracle's input generator and vector writer

Not a model: the support code of `lake exe oracle` (see `OracleExport/Main.lean`
and formal/README.md, "Differential oracle"). Nothing here is imported by
`LamboProofs`, and nothing under `LamboProofs/` imports it.

* `Gen` is a deterministic pseudo-random generator: SplitMix64 over a
  `UInt64` state, one fixed seed per model, so the vectors are a pure
  function of the Lean sources and regenerate byte for byte.
* `Vectors` is one model's vector file: metadata, then one JSON array of cases
  per modelled function, written one case per line with object keys sorted
  (`Lean.Json` keeps them in an ordered map), so a diff names the cases that
  changed.
-/
import Lean.Data.Json

namespace Lambo.Oracle

open Lean

/-! ## SplitMix64 -/

/-- The generator: SplitMix64's state. -/
abbrev Gen := StateM UInt64

/-- The next 64 random bits (SplitMix64, Steele, Lea and Flood 2014). -/
def nextU64 : Gen UInt64 := do
  let s := (← get) + 0x9E3779B97F4A7C15
  set s
  let z := (s ^^^ (s >>> 30)) * 0xBF58476D1CE4E5B9
  let z := (z ^^^ (z >>> 27)) * 0x94D049BB133111EB
  return z ^^^ (z >>> 31)

/-- A number in `[0, n)`; `0` when `n = 0`. The modulo bias is irrelevant
here: the inputs only need to be deterministic and varied. -/
def below (n : Nat) : Gen Nat := do
  if n = 0 then return 0
  return (← nextU64).toNat % n

/-- A number in `[lo, hi]`. -/
def range (lo hi : Nat) : Gen Nat := do
  return lo + (← below (hi + 1 - lo))

/-- `true` with probability `num / den`. -/
def chance (num den : Nat) : Gen Bool := do
  return (← below den) < num

/-- One element of a nonempty list. -/
def pick {α : Type} [Inhabited α] (xs : List α) : Gen α := do
  return xs.getD (← below xs.length) default

/-- `n` draws. -/
def many {α : Type} (n : Nat) (g : Gen α) : Gen (List α) := do
  let mut out := #[]
  for _ in [0:n] do
    out := out.push (← g)
  return out.toList

/-- A random permutation (Fisher-Yates). -/
def shuffle {α : Type} [Inhabited α] (xs : List α) : Gen (List α) := do
  let mut a := xs.toArray
  let n := a.size
  for k in [0:n] do
    let i := n - 1 - k
    let j ← below (i + 1)
    let x := a[i]!
    a := (a.set! i a[j]!).set! j x
  return a.toList

/-- Run a generator from a seed. -/
def run {α : Type} (seed : UInt64) (g : Gen α) : α := (g.run seed).1

/-! ## Vector files -/

/-- One modelled function's cases: the Lean name, the Rust it is compared
with, and the cases (`{"input": .., "output": ..}` objects). -/
structure Fn where
  lean : String
  rust : String
  cases : List Json

/-- One model's vector file. -/
structure Vectors where
  model : String
  seed : UInt64
  /-- How each abstract parameter of the model was instantiated, and any
  restriction on the inputs; free text, for people. -/
  notes : List String
  /-- Fixed data the cases share (a synonym table, a stemmer table), by
  name. -/
  tables : List (String × Json) := []
  fns : List Fn

/-- The file `formal/oracle/<model>.json`: a fixed key order, one case per
line, a trailing newline. -/
def Vectors.render (v : Vectors) : String := Id.run do
  let q (s : String) : String := (toJson s).compress
  let mut out := "{\n"
  out := out ++ s!"\"model\": {q v.model},\n"
  out := out ++ s!"\"lean\": {q s!"formal/LamboProofs/{v.model}.lean"},\n"
  out := out ++ s!"\"generator\": {q "formal/OracleExport/Main.lean"},\n"
  out := out ++ s!"\"seed\": {v.seed.toNat},\n"
  out := out ++ "\"notes\": [\n"
  out := out ++ ",\n".intercalate (v.notes.map q) ++ "\n],\n"
  out := out ++ s!"\"tables\": {(Json.mkObj v.tables).compress},\n"
  out := out ++ "\"functions\": [\n"
  let fns := v.fns.map fun f =>
    "{\"lean\": " ++ q f.lean ++ ", \"rust\": " ++ q f.rust ++ ", \"cases\": [\n" ++
      ",\n".intercalate (f.cases.map Json.compress) ++ "\n]}"
  out := out ++ ",\n".intercalate fns ++ "\n]\n}\n"
  return out

/-- A case object. -/
def case (input output : Json) : Json :=
  Json.mkObj [("input", input), ("output", output)]

/-- `Ordering` as the Rust test spells `std::cmp::Ordering`: `<`, `=`, `>`. -/
def ordChar : Ordering → Char
  | .lt => '<'
  | .eq => '='
  | .gt => '>'

/-- A comparator's table over `xs`: row `i` is `cmp xs[i] xs[j]` for every
`j`, as a string of `<`, `=`, `>`. -/
def cmpTable {α : Type} (cmp : α → α → Ordering) (xs : List α) : Json :=
  toJson (xs.map fun a => String.ofList (xs.map fun b => ordChar (cmp a b)))

end Lambo.Oracle
