/-
# Oracle vectors for `Expand.lean`

`Expand.expand nbrs depth seeds` over generated graphs. The model leaves the
neighbour function abstract; its header says what it stands for: for a
frontier node, the concatenation over `TRAVERSAL_ORDER` of
`out_neighbors_typed`, each id-ascending. The exporter builds `nbrs` that
way from the case's typed edges (`traversal`, `nbrsOf`) and writes it into
the input; the Rust test checks that the real graph's adjacency gives
exactly that list for every node before it runs `expand`, so the
instantiation is verified, not assumed. Edges of two non-traversable
concept-to-concept types are drawn too.

Members: `1..n` are concepts (id pair `(2, k)`), `0` the origin interaction,
`99` an id in no graph; seeds may name any, repeated. Only `required` is
compared: chunk-group siblings are not modelled (no concept has a group).
-/
import LamboProofs.Expand
import OracleExport.Gen

namespace Lambo.Oracle.Expand

open Lean Lambo.Expand Lambo.Oracle

/-- `TRAVERSAL_ORDER`. -/
def traversal : List String := ["Dependency", "Causal", "Hierarchical", "CoOccurrence", "Semantic"]

/-- Every edge type drawn: the traversable ones and two that are not. -/
def edgeTypes : List String := traversal ++ ["Contradicts", "SuspectedSupersedes"]

structure EdgeIn where
  src : Nat
  ty : String
  tgt : Nat
  deriving Inhabited, BEq

def EdgeIn.json (e : EdgeIn) : Json := Json.arr #[toJson e.src, toJson e.ty, toJson e.tgt]

/-- `nbrs x`: per traversable type in order, the targets id-ascending. -/
def nbrsOf (es : List EdgeIn) (x : Nat) : List Nat :=
  traversal.flatMap fun ty =>
    ((es.filter fun e => e.src == x && e.ty == ty).map (·.tgt)).mergeSort (· ≤ ·)

/-- A graph of `1..n` concepts with distinct edges and no self-loops. -/
def genEdges (n : Nat) : Gen (List EdgeIn) := do
  let m ← range (2 * n) (5 * n)
  let mut out : List EdgeIn := []
  for _ in [0:m] do
    let e : EdgeIn := { src := ← range 1 n, ty := ← pick edgeTypes, tgt := ← range 1 n }
    if e.src ≠ e.tgt && !out.contains e then out := out ++ [e]
  return out

def genCase : Gen Json := do
  let n ← pick [1, 2, 3, 4, 5, 6, 6, 8, 8, 10]
  let es ← genEdges n
  let seeds ← many (← pick [0, 1, 1, 2, 2, 3, 5]) (do
    match ← below 10 with
    | 0 => pure 0
    | 1 => pure 99
    | _ => range 1 n)
  let depth ← pick [0, 1, 2, 2, 3, 4]
  let nbrs := (List.range (n + 1)).filterMap fun x =>
    let l := nbrsOf es x
    if l.isEmpty then none else some (Json.arr #[toJson x, toJson l])
  return case
    (Json.mkObj [("concepts", toJson n), ("edges", toJson (es.map EdgeIn.json)),
      ("nbrs", Json.arr nbrs.toArray), ("seeds", toJson seeds), ("depth", toJson depth)])
    (Json.mkObj [("required", toJson (expand (nbrsOf es) depth seeds))])

def seed : UInt64 := 0x455850414E44 -- "EXPAND"

def vectors : Vectors := run seed do
  let cases ← many 250 genCase
  return {
    model := "Expand", seed
    notes := [
      "Members: 1..concepts are concepts (Uuid pair (2,k)), 0 the origin interaction, 99 an id in no graph.",
      "edges: [source, EdgeType, target], distinct, no self-loops; Contradicts and SuspectedSupersedes are not traversable.",
      "nbrs: Expand.expand's neighbour function, as the model's header describes it: per TRAVERSAL_ORDER type, out-neighbours id-ascending. Listed per node with any; the Rust test checks out_neighbors_typed gives exactly these lists.",
      "required: ExpandedSet::required ids in order; scores and siblings are not modelled."
    ]
    fns := [
      { lean := "Lambo.Expand.expand", rust := "recall::expand::expand", cases }
    ]
  }

end Lambo.Oracle.Expand
