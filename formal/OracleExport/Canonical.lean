/-
# Oracle vectors for `Canonical.lean`

The canonical key with the model's concrete tables, `Canonical.stop0` (the
real `STOPWORDS`) and `Canonical.stem0` (a lookup table that agrees with
Snowball English on its words, identity elsewhere). The stemmer is Rust logic
(`rust-stemmers`) the model leaves abstract, so the oracle does not
reimplement it: every generated input is built so that every token it stems
is a word of `Canonical.stemTable` (the Rust test first checks the real
stemmer against that table, word by word). Tokens that are identifiers
(`Canonical.isIdent`) and stopwords are free.

The model starts after the invisible-character strip and NFC, and lowercases
ASCII only, so the inputs are ASCII plus whitespace and separators that both
steps leave alone (`U+00A0`, `U+2003`, `U+3000`, `U+0085`), on which the
model is exact.

* `canonicalKeyF` (after #301) against `canonical_key`, and `canonicalKey`
  (before #301) against `unfiltered_canonical_key`, both with the synonym
  table `synonyms` (exact lookup on the trimmed raw text);
* `nameKeyF` against `name_key`, with `sizeOk` true and `isUuid` false: the
  name inputs pass `check_size` and are never UUID-shaped (both checked on the
  Rust side); the 8-hex-digit rule is the model's own.
-/
import LamboProofs.Canonical
import OracleExport.Gen

namespace Lambo.Oracle.Canonical

open Lean Lambo.Canonical Lambo.Oracle

/-- The synonym table: raw spelling to target. -/
def synonyms : List (String × String) :=
  [("register_user", "create_user"), ("LeaseWriter", "single writer"),
   ("ons", "the-lease"), ("XML parser", "xmlparser v2")]

def syn (t : Text) : Option Text :=
  (synonyms.find? (fun p => p.1.toList == t)).map (·.2.toList)

/-- The stemmed words: the keys of `Canonical.stemTable`. -/
def stemWords : List String := stemTable.map (·.1)

def identifiers : List String :=
  ["v2", "2122de39e47e", "2122de39e47", "src/main.rs", "a.b", "0.14.0", "x86", "issue#291"]

/-- A word in one of its spellings: as is, capitalized, or upper case. -/
def spell (w : String) : Gen String := do
  match ← below 4 with
  | 0 => return w.capitalize
  | 1 => return w.toUpper
  | _ => return w

def genPiece : Gen String := do
  match ← below 10 with
  | 0 | 1 => spell (← pick stopwords)
  | 2 | 3 => spell (← pick identifiers)
  | _ => spell (← pick stemWords)

/-- Separator runs. `names` leaves out the controls `check_size` refuses. -/
def seps (names : Bool) : List String :=
  [" ", " ", "-", "_", "\t", "\n", "--", " _ ", " ", " ", "　"] ++
    (if names then [] else ["\u0085", "\r", "\u000B"])

def isLowerS (s : String) : Bool := s.toList.getLast?.any isLowerA
def isUpperFirst (s : String) : Bool := s.toList.head?.any isUpperA

/-- Pieces joined by separator runs, or with no separator where the camelCase
split will cut (a lowercase end before an uppercase start). -/
def genText (names : Bool) : Gen String := do
  let ps ← many (← range 0 5) genPiece
  let mut out := ""
  let mut first := true
  for p in ps do
    if first then
      out := p
      first := false
    else if isLowerS out && isUpperFirst p && (← chance 1 2) then
      out := out ++ p
    else
      out := out ++ (← pick (seps names)) ++ p
  let pad (names : Bool) : Gen String := do
    if ← chance 1 4 then pick (if names then [" ", "\t", "\n", "　"] else [" ", "\u0085", " "])
    else return ""
  return (← pad names) ++ out ++ (← pad names)

/-- Every word the stemmer sees is a `stemTable` word. -/
def stemsKnown (s : String) : Bool :=
  let ws := ((words (camel false s.toList)).map (List.map lowerA)).filter (fun t => !stop0 t)
  ws.all fun w => isIdent w || stemWords.any (·.toList == w)

/-- A generated text whose stemmed words are all known (redrawn otherwise). -/
def genKnown (names : Bool) : Gen String := do
  for _ in [0:100] do
    let s ← genText names
    if stemsKnown s then return s
  return ""

def genKeyInput : Gen String := do
  match ← below 12 with
  | 0 => return (← pick ["", " ", "\t\n", "-_-"]) ++ (← pick (synonyms.map (·.1)))
  | 1 => pick ["Register_user", "register_user ", " LeaseWriter", "XML  parser", "the", "''s ''s",
      "''s-lease", "leas", "ons", ""]
  | _ => genKnown false

def genName : Gen String := do
  match ← below 10 with
  | 0 => pick ["deadbeef", "DEADBEEF", " cafebabe\t", "1a2b3c4d", "1a2b3c4", "1a2b3c4d5", "the-a-an",
      "''s ''s", "", " "]
  | _ => genKnown true

def key (s : String) : String := String.ofList (canonicalKeyF stop0 stem0 syn s.toList)
def unfiltered (s : String) : String := String.ofList (canonicalKey stop0 stem0 syn s.toList)
def name (s : String) : Option String :=
  (nameKeyF (fun _ => true) (fun _ => false) stop0 stem0 s.toList).map String.ofList

def seed : UInt64 := 0x43414E4F4E -- "CANON"

def vectors : Vectors := run seed do
  let keys ← many 300 genKeyInput
  let names ← many 200 genName
  let one (f : String → Json) (s : String) : Json := case (toJson s) (f s)
  return {
    model := "Canonical", seed
    notes := [
      "stop: Canonical.stop0 (STOPWORDS). stem: Canonical.stem0 (Canonical.stemTable, identity elsewhere). Every non-identifier, non-stopword token of every input is a stemTable word, so stem0 is the real stemmer on these inputs (the Rust test checks rust-stemmers against stemTable).",
      "Inputs are ASCII plus U+00A0, U+2003, U+3000 and U+0085, which the invisible strip and NFC leave alone and which ASCII lowercasing covers.",
      "syn: exact lookup of the trimmed raw text in the synonyms table below; name_key uses none.",
      "tables.synonyms is that table; tables.stem0 is Canonical.stemTable, which the Rust test checks against rust-stemmers before the replay.",
      "nameKeyF: sizeOk = true and isUuid = false; the name inputs pass check_size and are not UUIDs (the Rust test checks both). Output null is a refusal."
    ]
    tables := [
      ("synonyms", toJson (synonyms.map fun (k, v) => #[k, v])),
      ("stem0", toJson (stemTable.map fun (k, v) => #[k, v]))
    ]
    fns := [
      { lean := "Lambo.Canonical.canonicalKeyF", rust := "graph::canonical::canonical_key",
        cases := keys.map (one (toJson ∘ key)) },
      { lean := "Lambo.Canonical.canonicalKey", rust := "graph::canonical::unfiltered_canonical_key",
        cases := keys.map (one (toJson ∘ unfiltered)) },
      { lean := "Lambo.Canonical.nameKeyF", rust := "graph::graph::names::name_key",
        cases := names.map (one (toJson ∘ name)) }
    ]
  }

end Lambo.Oracle.Canonical
