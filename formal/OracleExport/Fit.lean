/-
# Oracle vectors for `Fit.lean`

`Fit.fitBlocks` over `Fit.rustEnv` (the Rust separator, ellipsis, newline and
`char::is_whitespace`) and `Fit.rustBlock` (the real truncation marker), with
the estimator named in each case:

* `default`: `Fit.defaultTokenCount`, `ceil(bytes / 3.5)` as `Fit.defaultG`,
  against `default_token_count`;
* `bytes`: the UTF-8 byte count (`str::len`);
* `chars`: the char count (`str::chars().count()`);
* `saturated`: `usize::MAX` for every text (`Fit.usizeMax`).

Text is a list of chars, so every cut the model makes is a char boundary; the
Rust test rebuilds each `ContextBlock` from its three parts, with
`content_len` and `label_end` their UTF-8 byte offsets. Budgets are drawn
around the thresholds that matter: each prefix of whole blocks, the marker
alone, and random values.
-/
import LamboProofs.Fit
import OracleExport.Gen

namespace Lambo.Oracle.Fit

open Lean Lambo.Fit Lambo.Oracle

/-- One block's parts. -/
structure Part where
  content : String
  label : String
  warn : String
  id : String
  scope : Option String
  deriving Inhabited

def Part.block (p : Part) : Block Char :=
  rustBlock p.content.toList p.label.toList p.warn.toList p.id.toList (p.scope.map String.toList)

def Part.json (p : Part) : Json :=
  Json.mkObj [("content", toJson p.content), ("label", toJson p.label), ("warn", toJson p.warn),
    ("inspect_id", toJson p.id), ("scope", toJson p.scope)]

def estimator : String → List Char → Nat
  | "bytes" => wsum Char.utf8Size
  | "chars" => List.length
  | "saturated" => fun _ => usizeMax
  | _ => defaultTokenCount

/-- Content characters: ASCII letters, whitespace of one, two and three UTF-8
bytes, and letters of two, three and four bytes. -/
def contentChars : List Char :=
  ['y', 'y', 'y', 'a', 'Q', ' ', ' ', '\t', '\n', ' ', ' ', 'é', '中', '😀']

def hex : List Char := "0123456789abcdef".toList

def genContent : Gen String := do
  let n ← if ← chance 1 5 then range 120 240 else range 0 60
  return String.ofList (← many n (pick contentChars))

def labels : List String := [
  " [Entity] (score 0.50, id 1a2b3c4d)",
  " [Entity, canonical] (score 0.50, id 1a2b3c4d, blast radius 3)",
  " [Logic] (score 1.00, id ffff0000, from sess-b)",
  ""
]

def warns : List String := [
  "",
  "",
  "\n⚑ Load-bearing pillar — 3 nodes depend on this. Modify with caution.",
  "\n⚠ Contradicted by 1a2b3c4d.\n⚠ Possibly superseded.",
  "\nw"
]

def genPart : Gen Part := do
  let id := String.ofList (← many 8 (pick hex))
  let scope ← if ← chance 1 4 then pure (some "sess-b") else pure none
  return { content := ← genContent, label := ← pick labels, warn := ← pick warns, id, scope }

/-- The thresholds of a block list: every prefix of whole blocks, and each
truncation form of the first block at cut `0` (for `ContentOnly`, the marker
alone). -/
def thresholds (est : List Char → Nat) (bs : List (Block Char)) : List Nat :=
  let prefixes := (List.range bs.length).map fun k =>
    est (ctxOf ['\n', '\n'] ((bs.take (k + 1)).map Block.text))
  let firsts := match bs with
    | [] => []
    | b :: _ => (forms b).map fun f => est (render (rustEnv est) b f 0)
  prefixes ++ firsts

/-- A budget near a threshold, at an extreme, or anywhere below twice the
largest threshold; at most `usize::MAX`, as `max_tokens` is a `usize`. -/
def genBudget (est : List Char → Nat) (bs : List (Block Char)) : Gen Nat := do
  return min usizeMax (← rawBudget)
where rawBudget : Gen Nat := do
  let ts := thresholds est bs
  let top := ts.foldl max 1
  match ← below 6 with
  | 0 => pick [0, 1, usizeMax - 1, usizeMax]
  | 1 => range 0 (2 * top)
  | _ =>
    let t ← pick (if ts.isEmpty then [0] else ts)
    let d ← pick [0, 0, 1, 2]
    if ← chance 1 2 then return t + d else return t - d

def formName : Form → String
  | .withWarnings => "with_warnings"
  | .labelOnly => "label_only"
  | .contentOnly => "content_only"

def genCase : Gen Json := do
  let parts ← many (← pick [0, 1, 1, 1, 2, 2, 3, 4]) genPart
  let estName ← pick ["default", "default", "default", "default", "bytes", "bytes", "chars", "chars", "saturated"]
  let est := estimator estName
  let bs := parts.map Part.block
  let maxT ← genBudget est bs
  let r := fitBlocks (rustEnv est) bs maxT
  return case
    (Json.mkObj [("blocks", toJson (parts.map Part.json)), ("estimator", toJson estName),
      ("max_tokens", toJson maxT)])
    (Json.mkObj [("context", toJson (String.ofList r.context)), ("whole", toJson r.whole),
      ("truncation", toJson (r.truncation.map formName))])

def seed : UInt64 := 0x464954 -- "FIT"

def vectors : Vectors := run seed do
  let cases ← many 220 genCase
  return {
    model := "Fit", seed
    notes := [
      "env: Fit.rustEnv (SEPARATOR, ELLIPSIS, the newline before the marker, char::is_whitespace); blocks: Fit.rustBlock (the real truncation_marker).",
      "estimator: default = Fit.defaultTokenCount (default_token_count), bytes = str::len, chars = chars().count(), saturated = usize::MAX.",
      "A block is its three parts content, label, warn; text is their concatenation, content_len and label_end their UTF-8 byte offsets.",
      "usize::MAX is 2^64 - 1: the vectors assume a 64-bit target, as Fit.usizeMax does."
    ]
    fns := [
      { lean := "Lambo.Fit.fitBlocks", rust := "recall::format::fit_blocks", cases }
    ]
  }

end Lambo.Oracle.Fit
