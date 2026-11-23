/-
# Oracle vectors for `Order.lean`

`Order.rankMembers` and `Order.tieBreak` are comparators with their primitive
orders left abstract. The oracle instantiates them as the Rust documents:

* `scoreCmp` is `f64::total_cmp` (IEEE 754 `totalOrder`), on the score's bit
  pattern: a negative number has every bit but the sign flipped, the result is
  compared as a signed integer (`totalCmpBits`). Scores are written as their
  64-bit patterns, so NaN, `-0.0` and the infinities are exact.
* `keyCmp` is `str::cmp` on the key, which is code-point lexicographic order
  (`charsCmp`; UTF-8 byte order is the same order). The Rust compares the
  *tie form* of a key, which differs from the key only under the scoped
  prefix `RepoResource:v1:`; `tie_form` is Rust logic the model does not
  define, so the generated keys never carry that prefix (the Rust test checks
  `tie_form(k) == k` for every key).
* `idCmp` is `natCmp` on small member numbers; the Rust test maps member `0`
  to the origin interaction (pair `(1, 1)`) and member `n > 0` to the concept
  id pair `(2, n)`, both of which order as the numbers do.
* `key` and `isCanonical` are the case's table: a member with a key is a
  concept with that `canonical_key`; a keyless member other than `0` is an id
  in no graph. `isCanonical` is "status is `canonical`".
-/
import LamboProofs.Order
import OracleExport.Gen

namespace Lambo.Oracle.Order

open Lean Lambo.Order Lambo.Oracle

/-- `f64::total_cmp` on bit patterns: the integer `totalOrder` key, made
unsigned by flipping the sign bit. -/
def totalKey (bits : UInt64) : UInt64 :=
  if bits >>> 63 == 1 then ~~~bits else bits ^^^ 0x8000000000000000

def totalCmpBits (a b : UInt64) : Ordering := natCmp (totalKey a).toNat (totalKey b).toNat

/-- `str::cmp`: lexicographic by code point. -/
def charsCmp : List Char → List Char → Ordering
  | [], [] => .eq
  | [], _ :: _ => .lt
  | _ :: _, [] => .gt
  | a :: as, b :: bs =>
    if a.toNat < b.toNat then .lt else if b.toNat < a.toNat then .gt else charsCmp as bs

/-- One member: its number, key, status (`none` when keyless), score bits. -/
structure Member where
  id : Nat
  key : Option String
  status : Option String
  bits : UInt64
  deriving Inhabited

def Member.json (m : Member) : Json :=
  Json.arr #[toJson m.id, toJson m.key, toJson m.status, toJson m.bits.toNat]

/-- Score bit patterns: ordinary values, both zeros, quiet and signalling NaN
of both signs, the infinities, the extremes. -/
def specialBits : List UInt64 :=
  [ 0x3FE0000000000000  -- 0.5
  , 0x3FD0000000000000  -- 0.25
  , 0x3FF0000000000000  -- 1.0
  , 0xBFF0000000000000  -- -1.0
  , 0x0000000000000000  -- 0.0
  , 0x8000000000000000  -- -0.0
  , 0x7FF8000000000000  -- NaN
  , 0xFFF8000000000000  -- -NaN
  , 0x7FF0000000000001  -- signalling NaN
  , 0x7FF0000000000000  -- inf
  , 0xFFF0000000000000  -- -inf
  , 0x0000000000000001  -- smallest subnormal
  , 0x8000000000000001  -- its negation
  , 0x7FEFFFFFFFFFFFFF  -- f64::MAX
  , 0xFFEFFFFFFFFFFFFF  -- f64::MIN
  ]

/-- Key characters: ASCII of both cases, `:`, and non-ASCII of two, three and
four UTF-8 bytes, so code-point and byte order are both exercised. -/
def keyChars : List Char := ['a', 'A', 'b', 'Z', ':', 'é', '中', '𝄞']

def genKey : Gen String := do
  if ← chance 1 10 then
    return "reporesource:v1:" ++ String.ofList [← pick keyChars]
  let n ← below 4
  return String.ofList (← many n (pick keyChars))

/-- A case's members: distinct numbers from `0..12` in random order; a small
pool of keys and scores per case, so ties on score and on key are common. -/
def genMembers : Gen (List Member) := do
  let n ← range 0 8
  let ids := (← shuffle (List.range 13)).take n
  let keys ← many (← range 1 3) genKey
  let pool ← many (← range 1 3) (do
    if ← chance 1 3 then return (← nextU64) else pick specialBits)
  ids.mapM fun id => do
    let bits ← pick pool
    if id == 0 || (← chance 1 4) then
      return { id, key := none, status := none, bits }
    let status ← pick ["none", "candidate", "venerable", "canonical", "canonical"]
    return { id, key := some (← pick keys), status := some status, bits }

def lookup (ms : List Member) (i : Nat) : Member :=
  (ms.find? (·.id == i)).getD default

def keyOf (ms : List Member) (i : Nat) : Option (List Char) :=
  (lookup ms i).key.map String.toList

def tieOf (ms : List Member) : Nat → Nat → Ordering :=
  tieBreak charsCmp (keyOf ms) natCmp

def rankOf (ms : List Member) : Nat → Nat → Ordering :=
  rankMembers (fun i => (lookup ms i).status == some "canonical") totalCmpBits
    (fun i => (lookup ms i).bits) charsCmp (keyOf ms) natCmp

/-- Sort by a comparator (any stable sort: `Order.sorted_unique`). -/
def sortBy (cmp : Nat → Nat → Ordering) (xs : List Nat) : List Nat :=
  xs.mergeSort (fun a b => cmp a b != .gt)

def rankCase (ms : List Member) : Json :=
  let ids := ms.map (·.id)
  case (Json.mkObj [("members", toJson (ms.map Member.json))])
    (Json.mkObj [("cmp", cmpTable (rankOf ms) ids), ("sorted", toJson (sortBy (rankOf ms) ids))])

def tieCase (ms : List Member) : Json :=
  let ids := ms.map (·.id)
  let keyed := ms.map fun m => Json.arr #[toJson m.id, toJson m.key]
  case (Json.mkObj [("members", Json.arr keyed.toArray)])
    (Json.mkObj [("cmp", cmpTable (tieOf ms) ids)])

def seed : UInt64 := 0x4F52444552 -- "ORDER"

def vectors : Vectors := run seed do
  let rank ← many 250 genMembers
  let tie ← many 150 genMembers
  return {
    model := "Order", seed
    notes := [
      "scoreCmp: f64::total_cmp on the score's bit pattern (members carry the bits as an integer).",
      "keyCmp: str::cmp, code-point lexicographic order. Keys never start with RepoResource:v1:, so tie_form is the identity on them (the Rust test checks it).",
      "idCmp: natCmp on member numbers. Member 0 is the origin interaction (Uuid pair (1,1)), member n > 0 is Uuid pair (2,n).",
      "key / isCanonical: a member [n, key, status, bits] with a key is a concept with that canonical_key and status; a keyless member other than 0 is an id in no graph. isCanonical is status == canonical.",
      "cmp: row i, column j is the comparator on members i and j in input order, as < = >. sorted: member numbers in the comparator's order."
    ]
    fns := [
      { lean := "Lambo.Order.rankMembers", rust := "recall::assemble::rank_cmp, rank_members", cases := rank.map rankCase },
      { lean := "Lambo.Order.tieBreak", rust := "types::tie_break_by_key", cases := tie.map tieCase }
    ]
  }

end Lambo.Oracle.Order
