/-
# Oracle vectors for `TopK.lean`

`TopK.emit` (no selection) and `TopK.emitR` / `TopK.skipsR` (the #181
redundancy skip, with `win = TopK.window topK` and `wmax = TopK.windowMax`)
over generated ranked lists. The parameters:

* `isConcept`: a member with a content class is a concept; member `0` is the
  origin interaction and any other classless member an id in no graph.
* `forced`, `warns`: per member, from the case.
* `red` (the pair verdict, `Selection::verdict`): the model leaves it
  abstract and the Rust computes it from `Redundancy::signal`, which the
  model does not define. It is instantiated as "same content class", and
  each class is a set of contents with one `normalized_content`; the Rust
  test first checks, for every pair of concepts in every case, that
  `Redundancy::signal` finds the pair redundant exactly when the classes are
  equal (no member carries a vector or a `Semantic` edge, so
  `similar_signals` adds nothing).

`skipsR` lists `(kept, skipped)` in scan order; the Rust keeps them in a map
from the kept hit, so the test orders them by the skipped member's rank.
-/
import LamboProofs.TopK
import OracleExport.Gen

namespace Lambo.Oracle.TopK

open Lean Lambo.TopK Lambo.Oracle

/-- Contents by class: each class's spellings differ in whitespace only, so
they share one `normalized_content` (which keeps case). -/
def contents : List (List String) :=
  [["alpha beta", "alpha  beta", " alpha\tbeta "], ["gamma"], ["delta 7", "delta\n7"],
   ["Alpha beta"]]

/-- One ranked entry: member number, content class and spelling (none: not a
concept), forced, warns. -/
structure Entry where
  id : Nat
  cls : Option Nat
  content : Option String
  forced : Bool
  warns : Bool
  deriving Inhabited

/-- The spellings of class `c`: the four fixed families, then `item c` for
the wide cases' many distinct classes. -/
def spellings (c : Nat) : List String :=
  if c < contents.length then contents.getD c [] else [s!"item {c}", s!"item  {c}"]

def Entry.json (e : Entry) : Json :=
  Json.arr #[toJson e.id, toJson e.cls, toJson e.content, toJson e.forced, toJson e.warns]

/-- A ranked list: distinct members, mostly concepts of the first class (so
the skip fires), some forced, a few warned; often a forced concept of the
first class first, so the window binds. -/
def genEntries : Gen (List Entry) := do
  let n ← range 0 16
  let ids := (← shuffle (List.range (n + 3))).take (← range 0 (n + 3))
  let es ← ids.mapM fun id => do
    let concept := id ≠ 0 && id ≤ n
    let cls ← if concept then (do
        if ← chance 3 4 then pure (some 0) else pure (some (← below contents.length)))
      else pure none
    let content ← match cls with
      | some c => pure (some (← pick (spellings c)))
      | none => pure none
    return { id, cls, content, forced := ← chance 3 10, warns := ← chance 1 10 }
  if ← chance 4 5 then
    match es.findIdx? (·.cls == some 0) with
    | some i => return { es[i]! with forced := true } :: es.eraseIdx i
    | none => return es
  return es

/-- A wide list, so `taken` reaches `TopK.windowMax` (`WINDOW_MAX`): 21 to
30 forced concepts of distinct classes first (every one is emitted and
taken while there is room), then a few ordinary members, each redundant with
one of them (often one past the first `windowMax`, which the cap keeps out of
`taken`) or with none, some warned. -/
def genWide : Gen (List Entry) := do
  let m ← range 21 30
  let forced := (List.range m).map fun i =>
    ({ id := i + 1, cls := some (100 + i), content := some s!"item {100 + i}", forced := true,
       warns := false } : Entry)
  let k ← range 1 6
  let tail ← (List.range k).mapM fun j => do
    let c ← match ← below 6 with
      | 0 => pure (200 + j)
      | 1 | 2 | 3 => pure (100 + (← range 18 (m - 1)))
      | _ => pure (100 + (← range 5 (m - 1)))
    return ({ id := m + 1 + j, cls := some c, content := some (← pick (spellings c)),
              forced := false, warns := ← chance 1 8 } : Entry)
  return forced ++ tail

def isConcept (es : List Entry) (i : Nat) : Bool := (es.find? (·.id == i)).any (·.cls.isSome)
def forcedOf (es : List Entry) (i : Nat) : Bool := (es.find? (·.id == i)).any (·.forced)
def warnsOf (es : List Entry) (i : Nat) : Bool := (es.find? (·.id == i)).any (·.warns)
def clsOf (es : List Entry) (i : Nat) : Option Nat := (es.find? (·.id == i)).bind (·.cls)
def red (es : List Entry) (a b : Nat) : Bool := clsOf es a == clsOf es b

def genTopK : Gen Nat := pick [0, 1, 1, 1, 1, 2, 2, 3, 5]

def input (es : List Entry) (topK : Nat) (similar : Option Bool) : Json :=
  Json.mkObj [("ranked", toJson (es.map Entry.json)), ("top_k", toJson topK),
    ("similar_signals", toJson similar)]

def emitCase (es : List Entry) (topK : Nat) : Json :=
  let ids := es.map (·.id)
  case (input es topK none)
    (Json.mkObj [("hits", toJson (emit (isConcept es) (forcedOf es) topK 0 ids))])

def emitRCase (es : List Entry) (topK : Nat) (similar : Bool) : Json :=
  let ids := es.map (·.id)
  let args := (isConcept es, forcedOf es, warnsOf es, red es)
  let hits := emitR args.1 args.2.1 args.2.2.1 args.2.2.2 topK (window topK) windowMax 0 0 [] ids
  let skips := skipsR args.1 args.2.1 args.2.2.1 args.2.2.2 topK (window topK) windowMax 0 0 [] ids
  case (input es topK (some similar))
    (Json.mkObj [("hits", toJson hits), ("skips", toJson (skips.map fun (k, s) => #[k, s]))])

def seed : UInt64 := 0x544F504B -- "TOPK"

def vectors : Vectors := run seed do
  let plain ← many 150 (do return emitCase (← genEntries) (← genTopK))
  let sel ← many 250 (do return emitRCase (← genEntries) (← genTopK) (← chance 1 2))
  let wide ← many 60 (do return emitRCase (← genWide) (← pick [1, 1, 2, 3, 5]) (← chance 1 2))
  return {
    model := "TopK", seed
    notes := [
      "ranked: [member, class, content, forced, warns] in rank order. Member 0 is the origin interaction; a member with a class is a concept with that content; any other classless member is an id in no graph.",
      "isConcept: has a class. forced / warns: the flags. red (Selection::verdict): same class; the Rust test checks Redundancy::signal agrees on every pair of concepts.",
      "emitR / skipsR: win = TopK.window top_k, wmax = TopK.windowMax, from slots = 0, checked = 0, prev = []. Their cases share inputs; similar_signals is the Redundancy flag (no vectors or Semantic edges, so it changes nothing).",
      "skips: (kept, skipped) in scan order; the Rust map is flattened and ordered by the skipped member's rank.",
      "The last 60 emitR cases are wide: 21 to 30 forced concepts of distinct classes, so the emitted list passes windowMax and the cap on taken matters."
    ]
    fns := [
      { lean := "Lambo.TopK.emit", rust := "recall::assemble::emitted (selection None)", cases := plain },
      { lean := "Lambo.TopK.emitR", rust := "recall::assemble::emitted (selection Some)", cases := sel ++ wide }
    ]
  }

end Lambo.Oracle.TopK
