/-
# Canonical keys and durable names: what makes two spellings the same

Model of the canonical-key pipeline in `src/graph/canonical.rs` (at `a15d1e72`,
and the fix for #301, item 1, which drops empty stems):

* `split_camel_case`: a space before every ASCII uppercase letter whose
  predecessor is an ASCII lowercase letter, on the original case;
* `tokens(content, true)` (`normalize_tokens` before the #301 fix; after it,
  `normalize_tokens` is `tokensF`): split on `-`, `_` and
  `char::is_whitespace`, drop empty pieces, lowercase, drop `STOPWORDS`, then
  stem every token with Snowball English (Porter2, `rust-stemmers`
  `Algorithm::English`; the Rust docs called it "Porter" until #302) except
  an identifier token (one containing an ASCII digit, `/` or `.`,
  `is_identifier_token`, #148), which is kept as lowercased. The camelCase split runs first, so an
  identifier piece with a lower-to-upper boundary inside is still split;
  empty stems are not filtered out (`tokens`; the fix for #301 drops them,
  `tokensF`);
* `tokens_to_key`: sort the tokens (`Vec<String>::sort`) and join them with
  single spaces;
* `canonical_key(content, synonyms)`: trim, look the trimmed raw text up in the
  synonym table once (direct lookup only), then the steps above;

and of `name_key` in `src/graph/graph/names.rs`, which validates a durable name
with `check_size`, derives `canonical_key(name, |_| None)` (no synonyms), and
refuses the name when that key is empty or the trimmed name is id-shaped (a
UUID, or exactly 8 ASCII hex digits).

## Model

* Text is a list of Unicode scalar values (`List Char`). The model starts after
  the two steps it leaves out (below), so it is exact for ASCII input.
* The stopword list and the stemmer are parameters (`stop`, `stem`). Every
  structural theorem holds for any such pair; the ones that need more of the
  stemmer say so in a hypothesis (`Sane`, `Stable`). The concrete `stop0` is the
  real 13-word `STOPWORDS`; the concrete `stem0` is a small lookup table that
  agrees with Snowball English (Porter2) on the words the examples use
  (identity elsewhere). The real stemmer is neither `Stable` nor `Sane`, and
  neither is `stem0`, which keeps two of its failures (`leas` and `''s`); the
  theorems with those hypotheses reach the concrete tables only through the
  `decide` examples.
* `isWs` is Rust's `char::is_whitespace` (Unicode `White_Space`), exactly.
* `tokLe` orders tokens by code point, lexicographically. Rust sorts `String`s
  by UTF-8 bytes, which is the same order.
* `check_size` and `uuid::Uuid::parse_str` are parameters of `nameKey`
  (`sizeOk`, `isUuid`); the 8-hex-digit rule is modelled exactly.

The old `key` is also exactly `unfiltered_canonical_key`, which the
`lambo provision` re-key uses to recognise a key written between #148 and the
#301 fix.

## Results

Structure (any stopword list and stemmer):

* `key_perm`: inputs whose token lists are permutations of each other have the
  same key: the key ignores token order (`canonical_key_sorts_and_joins`).
  `key_swap`, `key_segments_perm`: reordering the pieces of a name around
  separators never changes its key.
* `key_sep_alike`, `key_sep_run`: `-`, `_` and every whitespace character are
  interchangeable, and a run of them counts as one (the
  `normalize_splits_hyphen_underscore_mix` test).
* `key_lower`, `key_lower_of_noLower`: lowercasing never changes the key of a
  spelling with no lower-to-upper (camelCase) boundary, in particular of an
  all-caps one. Case is not ignored in general: a camelCase boundary splits a
  word (`camel_not_lower`).
* `tokens_stopword`, `key_stop_segment`: a stopword piece contributes nothing,
  wherever it stands. `key_all_stop`: an input of only stopwords and separators
  has the empty key, and `nameKey_all_stop` / `nameKey_empty`: `name_key`
  refuses it.
* `key_eq_iff_perm` (`Sane` stemmer: nonempty, separator-free stems): two inputs
  have the same key exactly when their stems form the same multiset. The key is
  a multiset, not a set: `lease lease` is not `lease` (`dup_not_collapsed`).
  The real stemmer is not `Sane`: it stems `''s` to the empty string (the
  prelude drops the leading apostrophe, Step 1a removes `'s`), and `tokens`
  keeps empty stems, so `''s-lease` has the key ` leas` with a leading space
  and `''s ''s` the key ` ` (`empty_stem_key`). Since tokens never contain a
  separator, joining with spaces still tells token lists apart except "no
  tokens" from "one empty token", both of which give the empty key. `name_key`
  refuses the empty key, so for accepted names the conclusion of
  `nameKey_eq_iff_perm` still holds; only the proof here assumes `Sane`.
* `key_idem` (`Stable` stemmer: its outputs are fixed points, contain no
  separator or uppercase, and are not stopwords): normalizing a key gives the
  same key. `stable_id` shows the hypothesis is satisfiable. The real stemmer
  is not `Stable`, because stemming is not idempotent on ordinary words: `lease`
  stems to `leas` but `leas` stems to `lea`, so the key of `lease` is `leas`
  and the key of `leas` is `lea` (`leas_not_idempotent`). The canonical key is
  therefore not idempotent whatever the stopword order (`names.rs` `upgrade`
  says the same). A second, rarer case: `ons` stems to the stopword `on`, which
  is not removed because stopwords are dropped before stemming, so the key of
  `ons` is `on` while the key of `on` is empty (`ons_not_idempotent`).
* `canonicalKey_syn`: a synonym is followed once, on the trimmed raw text, and
  its target is then normalized like any input (no chains).

The fix for #301, item 1 (`tokensF`, `canonicalKeyF`, `keyF`, `nameKeyF`: the
old tokens with empty stems dropped). `tokens`, `key`, `nameKey` and every
theorem above stay, as statements about the previous version, including
`empty_stem_key`. The fix needs only `NoSep` (no separator inside a stem), the
half of `Sane` that the real stemmer and `stem0` meet (`noSep_stem0`):

* `canonicalKeyF_tidy`, `keyF_tidy`: a fixed key never has a leading, trailing
  or doubled space (`tidy`), whatever the synonym table; `keyF0_tidy` for the
  concrete tables.
* `keyF_eq_key`, `canonicalKeyF_eq_canonicalKey`, `nameKeyF_eq_nameKey`: where
  no stem is empty the fixed key and `name_key` agree with the old ones, so
  only a key with an empty stem changes; `keyF_eq_key_of_sane`: for a `Sane`
  stemmer nothing changes.
* `keyF_all_empty`, `nameKeyF_all_empty`: an input whose stems are all empty
  has the empty key, which `name_key`'s existing empty-key check refuses.
* `keyF_eq_iff_perm`: two inputs share a fixed key exactly when their nonempty
  stems form the same multiset, now with `NoSep` alone (the old
  `key_eq_iff_perm` needed `Sane`).
* `issue301_fixed`: `''s-lease` keys as `leas` (the name `lease`), and the name
  `''s ''s` keys as the empty key and is refused.

Names (`name_key`):

* `nameKey_eq_iff_key`: two names that pass `check_size` and are not id-shaped
  collide exactly when their canonical keys are equal; `nameKey_eq_iff_perm`:
  exactly when their stems form the same multiset.
* #290, item 2, as theorems about the current code: `issue290_order`
  (`single-writer-lease`, `lease writer single` and `SingleWriterLeases` are one
  name), `issue290_stopword` (`the-lease` is `lease`), and the general forms
  `nameKey_swap` and `nameKey_stop_segment`.

Boundary: the invisible-character strip and Unicode NFC that open `tokens`
(the model reads their output), Unicode case mapping beyond ASCII
(`str::to_lowercase` maps non-ASCII letters too; `lowerA` is ASCII-only), the
Snowball English algorithm itself (a parameter), `legacy_canonical_key`, and
`canonicalize` matching a key against the graph are not modelled. How a key is
bound and kept is `Names.lean`.
-/

namespace Lambo.Canonical

/-- Text after the invisible strip and NFC: a list of scalar values. -/
abbrev Text := List Char
/-- One token. -/
abbrev Token := List Char

/-! ## Character classes -/

/-- Rust `char::is_whitespace`: the Unicode `White_Space` property. -/
def isWsNat (n : Nat) : Bool :=
  (9 ≤ n && n ≤ 13) || n == 32 || n == 0x85 || n == 0xA0 || n == 0x1680 ||
  (0x2000 ≤ n && n ≤ 0x200A) || n == 0x2028 || n == 0x2029 || n == 0x202F ||
  n == 0x205F || n == 0x3000

/-- `char::is_whitespace`. -/
def isWs (c : Char) : Bool := isWsNat c.toNat

/-- The tokenizer's split set: `c == '-' || c == '_' || c.is_whitespace()`. -/
def isSep (c : Char) : Bool := c.toNat == 45 || c.toNat == 95 || isWs c

/-- `char::is_ascii_uppercase`. -/
def isUpperA (c : Char) : Bool := 65 ≤ c.toNat && c.toNat ≤ 90

/-- `char::is_ascii_lowercase`. -/
def isLowerA (c : Char) : Bool := 97 ≤ c.toNat && c.toNat ≤ 122

/-- ASCII lowercasing (the model's stand-in for `str::to_lowercase`). -/
def lowerA (c : Char) : Char := if isUpperA c then Char.ofNat (c.toNat + 32) else c

/-- An ASCII digit, `/` or `.`. -/
def isIdentChar (c : Char) : Bool := (48 ≤ c.toNat && c.toNat ≤ 57) || c.toNat == 47 || c.toNat == 46

/-- `is_identifier_token` (#148): kept as it is, not stemmed. -/
def isIdent (t : Token) : Bool := t.any isIdentChar

/-- `u8::is_ascii_hexdigit`. -/
def isHex (c : Char) : Bool :=
  (48 ≤ c.toNat && c.toNat ≤ 57) || (97 ≤ c.toNat && c.toNat ≤ 102) || (65 ≤ c.toNat && c.toNat ≤ 70)

theorem toNat_ofNat_small (n : Nat) (h : n < 55296) : (Char.ofNat n).toNat = n := by
  have hv : n.isValidChar := Or.inl h
  simp [Char.ofNat, hv, Char.ofNatAux, Char.toNat]

theorem lowerA_toNat (c : Char) :
    (lowerA c).toNat = if isUpperA c then c.toNat + 32 else c.toNat := by
  by_cases h : isUpperA c = true
  · simp only [lowerA, h, ite_true]
    simp [isUpperA] at h
    exact toNat_ofNat_small _ (by omega)
  · simp [lowerA, h]

theorem sep_not_upper {c : Char} (h : isSep c = true) : isUpperA c = false := by
  simp [isSep, isWs, isWsNat, isUpperA] at *
  omega

theorem sep_not_lower {c : Char} (h : isSep c = true) : isLowerA c = false := by
  simp [isSep, isWs, isWsNat, isLowerA] at *
  omega

theorem ws_sep {c : Char} (h : isWs c = true) : isSep c = true := by
  simp [isSep, h]

theorem upper_not_sep {c : Char} (h : isUpperA c = true) : isSep c = false := by
  cases hs : isSep c
  · rfl
  · rw [sep_not_upper hs] at h; cases h

theorem lowerA_of_not_upper {c : Char} (h : isUpperA c = false) : lowerA c = c := by
  simp [lowerA, h]

theorem lowerA_not_upper (c : Char) : isUpperA (lowerA c) = false := by
  have := lowerA_toNat c
  by_cases h : isUpperA c = true
  · simp only [h, ite_true] at this
    simp only [isUpperA, Bool.and_eq_true, decide_eq_true_eq] at h
    simp only [isUpperA, this]
    simp
    omega
  · simp at h
    rw [lowerA_of_not_upper h]; exact h

theorem lowerA_idem (c : Char) : lowerA (lowerA c) = lowerA c :=
  lowerA_of_not_upper (lowerA_not_upper c)

theorem isSep_lowerA (c : Char) : isSep (lowerA c) = isSep c := by
  by_cases h : isUpperA c = true
  · have h1 := upper_not_sep h
    have h2 : isSep (lowerA c) = false := by
      have := lowerA_toNat c
      simp only [h, ite_true] at this
      simp only [isUpperA, Bool.and_eq_true, decide_eq_true_eq] at h
      simp only [isSep, isWs, isWsNat, this]
      simp
      omega
    rw [h1, h2]
  · simp at h
    rw [lowerA_of_not_upper h]

/-! ## The camelCase split -/

/-- `split_camel_case`; the flag is "the previous character was ASCII lowercase". -/
def camel : Bool → Text → Text
  | _, [] => []
  | p, c :: cs =>
    if p && isUpperA c then ' ' :: c :: camel (isLowerA c) cs
    else c :: camel (isLowerA c) cs

theorem camel_noUpper : ∀ (p : Bool) (s : Text), (∀ c ∈ s, isUpperA c = false) → camel p s = s
  | _, [], _ => rfl
  | p, c :: cs, h => by
    have hc := h c (by simp)
    have ih := camel_noUpper (isLowerA c) cs (fun d hd => h d (by simp [hd]))
    simp [camel, hc, ih]

theorem camel_noLower : ∀ (s : Text), (∀ c ∈ s, isLowerA c = false) → camel false s = s
  | [], _ => rfl
  | c :: cs, h => by
    have hc := h c (by simp)
    have ih := camel_noLower cs (fun d hd => h d (by simp [hd]))
    simp [camel, hc, ih]

theorem camel_append_sep {c : Char} (hc : isSep c = true) (Y : Text) :
    ∀ (p : Bool) (X : Text), camel p (X ++ c :: Y) = camel p X ++ c :: camel false Y
  | p, [] => by simp [camel, sep_not_upper hc, sep_not_lower hc]
  | p, x :: xs => by
    have ih := camel_append_sep hc Y (isLowerA x) xs
    simp only [List.cons_append, camel, ih]
    split <;> simp

/-! ## Splitting into words -/

/-- Prepend a word unless it is empty (`.filter(|t| !t.is_empty())`). -/
def push (w : Token) (ws : List Token) : List Token :=
  match w with
  | [] => ws
  | _ :: _ => w :: ws

/-- The run before the first separator, and the words after it. -/
def wordsAux : Text → Token × List Token
  | [] => ([], [])
  | c :: cs =>
    if isSep c then ([], push (wordsAux cs).1 (wordsAux cs).2)
    else (c :: (wordsAux cs).1, (wordsAux cs).2)

/-- `s.split(|c| c == '-' || c == '_' || c.is_whitespace()).filter(|t| !t.is_empty())`. -/
def words (s : Text) : List Token := push (wordsAux s).1 (wordsAux s).2

theorem push_append (w : Token) (ws vs : List Token) : push w (ws ++ vs) = push w ws ++ vs := by
  cases w <;> rfl

theorem wordsAux_append_sep {c : Char} (hc : isSep c = true) (Y : Text) :
    ∀ X : Text, wordsAux (X ++ c :: Y) = ((wordsAux X).1, (wordsAux X).2 ++ words Y)
  | [] => by simp [wordsAux, hc, words]
  | x :: xs => by
    have ih := wordsAux_append_sep hc Y xs
    simp only [List.cons_append, wordsAux, ih]
    split <;> simp [push_append]

theorem words_append_sep {c : Char} (hc : isSep c = true) (X Y : Text) :
    words (X ++ c :: Y) = words X ++ words Y := by
  simp [words, wordsAux_append_sep hc, push_append]

theorem wordsAux_noSep : ∀ s : Text, (∀ c ∈ s, isSep c = false) → wordsAux s = (s, [])
  | [], _ => rfl
  | c :: cs, h => by
    have hc := h c (by simp)
    have ih := wordsAux_noSep cs (fun d hd => h d (by simp [hd]))
    simp [wordsAux, hc, ih]

theorem words_single (s : Text) (hne : s ≠ []) (h : ∀ c ∈ s, isSep c = false) :
    words s = [s] := by
  cases s with
  | nil => exact absurd rfl hne
  | cons a as => simp [words, wordsAux_noSep _ h, push]

/-- `stems.join(" ")` and its generalization to any separator. -/
def joinWith (c : Char) : List Text → Text
  | [] => []
  | [t] => t
  | t :: u :: us => t ++ c :: joinWith c (u :: us)

/-- `tokens_to_key`'s join. -/
abbrev joinSp : List Token → Text := joinWith ' '

/-- A token that the tokenizer could return: nonempty, no separator inside. -/
def Good (t : Token) : Prop := t ≠ [] ∧ ∀ c ∈ t, isSep c = false

theorem words_joinWith {c : Char} (hc : isSep c = true) :
    ∀ L : List Token, (∀ t ∈ L, Good t) → words (joinWith c L) = L
  | [], _ => rfl
  | [t], h => by
    have ht := h t (by simp)
    simpa [joinWith] using words_single t ht.1 ht.2
  | t :: u :: us, h => by
    have ht := h t (by simp)
    have ih := words_joinWith hc (u :: us) (fun v hv => h v (by simp_all))
    simp only [joinWith]
    rw [words_append_sep hc, ih, words_single t ht.1 ht.2]
    rfl

theorem joinWith_inj {c : Char} (hc : isSep c = true) {L M : List Token}
    (hL : ∀ t ∈ L, Good t) (hM : ∀ t ∈ M, Good t) (h : joinWith c L = joinWith c M) : L = M := by
  rw [← words_joinWith hc L hL, ← words_joinWith hc M hM, h]

theorem push_map (f : Char → Char) (w : Token) (ws : List Token) :
    push (w.map f) (ws.map (List.map f)) = (push w ws).map (List.map f) := by
  cases w <;> rfl

theorem wordsAux_map (f : Char → Char) (hf : ∀ c, isSep (f c) = isSep c) :
    ∀ s : Text, wordsAux (s.map f)
      = ((wordsAux s).1.map f, (wordsAux s).2.map (List.map f))
  | [] => rfl
  | c :: cs => by
    have ih := wordsAux_map f hf cs
    simp only [List.map_cons, wordsAux, ih, hf]
    split <;> simp [push_map]

theorem words_map (f : Char → Char) (hf : ∀ c, isSep (f c) = isSep c) (s : Text) :
    words (s.map f) = (words s).map (List.map f) := by
  simp [words, wordsAux_map f hf, push_map]

theorem mem_push {v w : Token} {ws : List Token} (h : v ∈ push w ws) :
    (v = w ∧ w ≠ []) ∨ v ∈ ws := by
  cases w with
  | nil => exact Or.inr h
  | cons a as =>
    simp only [push, List.mem_cons] at h
    rcases h with h | h
    · exact Or.inl ⟨h, by simp⟩
    · exact Or.inr h

theorem wordsAux_good : ∀ s : Text,
    (∀ c ∈ (wordsAux s).1, isSep c = false) ∧ (∀ w ∈ (wordsAux s).2, Good w)
  | [] => by simp [wordsAux]
  | c :: cs => by
    have ⟨ih1, ih2⟩ := wordsAux_good cs
    by_cases hc : isSep c = true
    · simp only [wordsAux, hc, ite_true]
      refine ⟨by simp, fun w hw => ?_⟩
      rcases mem_push hw with ⟨rfl, hne⟩ | hw
      · exact ⟨hne, ih1⟩
      · exact ih2 w hw
    · simp only [wordsAux, hc]
      simp only [Bool.false_eq_true, ite_false]
      refine ⟨fun d hd => ?_, ih2⟩
      simp only [List.mem_cons] at hd
      rcases hd with rfl | hd
      · simpa using hc
      · exact ih1 d hd

theorem words_good (s : Text) : ∀ w ∈ words s, Good w := by
  intro w hw
  have ⟨h1, h2⟩ := wordsAux_good s
  rcases mem_push hw with ⟨rfl, hne⟩ | hw
  · exact ⟨hne, h1⟩
  · exact h2 w hw

/-! ## Tokens -/

/-- Identifier tokens are kept; every other token is stemmed. -/
def stemOrKeep (stem : Token → Token) (t : Token) : Token := if isIdent t then t else stem t

/-- `tokens(content, true)` after the invisible strip and NFC: `normalize_tokens` before the
fix for #301 (after it, `tokensF`). -/
def tokens (stop : Token → Bool) (stem : Token → Token) (s : Text) : List Token :=
  (((words (camel false s)).map (List.map lowerA)).filter (fun t => !stop t)).map (stemOrKeep stem)

variable {stop : Token → Bool} {stem : Token → Token}

theorem tokens_nil : tokens stop stem [] = [] := rfl

/-- Locality: a separator cuts the input into two independent halves. -/
theorem tokens_append_sep {c : Char} (hc : isSep c = true) (X Y : Text) :
    tokens stop stem (X ++ c :: Y) = tokens stop stem X ++ tokens stop stem Y := by
  simp [tokens, camel_append_sep hc, words_append_sep hc, List.filter_append]

theorem tokens_sep_cons {c : Char} (hc : isSep c = true) (Y : Text) :
    tokens stop stem (c :: Y) = tokens stop stem Y := by
  simpa [tokens_nil] using tokens_append_sep (stop := stop) (stem := stem) hc [] Y

theorem tokens_all_sep : ∀ s : Text, (∀ c ∈ s, isSep c = true) → tokens stop stem s = []
  | [], _ => rfl
  | c :: cs, h => by
    rw [tokens_sep_cons (h c (by simp))]
    exact tokens_all_sep cs (fun d hd => h d (by simp [hd]))

theorem tokens_joinWith {c : Char} (hc : isSep c = true) :
    ∀ segs : List Text, tokens stop stem (joinWith c segs) = segs.flatMap (tokens stop stem)
  | [] => rfl
  | [t] => by simp [joinWith]
  | t :: u :: us => by
    have ih := tokens_joinWith hc (u :: us)
    simp only [joinWith]
    rw [tokens_append_sep hc, ih]
    simp

/-! ## Trimming -/

/-- `str::trim_end`. -/
def trimEnd : Text → Text
  | [] => []
  | c :: cs => if isWs c && (trimEnd cs).isEmpty then [] else c :: trimEnd cs

/-- `str::trim`. -/
def trim (s : Text) : Text := trimEnd (s.dropWhile isWs)

theorem tokens_dropWhile : ∀ s : Text, tokens stop stem (s.dropWhile isWs) = tokens stop stem s
  | [] => rfl
  | c :: cs => by
    by_cases h : isWs c = true
    · simp only [List.dropWhile, h]
      rw [tokens_sep_cons (ws_sep h)]
      exact tokens_dropWhile cs
    · simp [List.dropWhile, h]

theorem trimEnd_split : ∀ s : Text, ∃ B : Text, s = trimEnd s ++ B ∧ ∀ b ∈ B, isWs b = true
  | [] => ⟨[], rfl, by simp⟩
  | c :: cs => by
    obtain ⟨B, hB, hws⟩ := trimEnd_split cs
    by_cases h : isWs c = true ∧ trimEnd cs = []
    · refine ⟨c :: cs, ?_, ?_⟩
      · simp [trimEnd, h.1, h.2]
      · intro b hb
        simp only [List.mem_cons] at hb
        rcases hb with rfl | hb
        · exact h.1
        · rw [hB, h.2] at hb; exact hws b (by simpa using hb)
    · refine ⟨B, ?_, hws⟩
      have : trimEnd (c :: cs) = c :: trimEnd cs := by
        simp only [trimEnd]
        have hn : ¬ (isWs c && (trimEnd cs).isEmpty) = true := by
          intro h'
          simp only [Bool.and_eq_true, List.isEmpty_iff] at h'
          exact h h'
        simp only [hn, Bool.false_eq_true, ite_false]
      rw [this, List.cons_append, ← hB]

theorem tokens_trimEnd (s : Text) : tokens stop stem (trimEnd s) = tokens stop stem s := by
  obtain ⟨B, hB, hws⟩ := trimEnd_split s
  calc tokens stop stem (trimEnd s) = tokens stop stem (trimEnd s ++ B) := by
        cases B with
        | nil => simp
        | cons b bs =>
          rw [tokens_append_sep (ws_sep (hws b (by simp))),
            tokens_all_sep bs (fun d hd => ws_sep (hws d (by simp [hd])))]
          simp
    _ = tokens stop stem s := by rw [← hB]

theorem tokens_trim (s : Text) : tokens stop stem (trim s) = tokens stop stem s := by
  simp [trim, tokens_trimEnd, tokens_dropWhile]

/-! ## Sorting -/

/-- Lexicographic order by code point (= Rust's byte order on UTF-8 `String`s). -/
def tokLe : Token → Token → Bool
  | [], _ => true
  | _ :: _, [] => false
  | a :: as, b :: bs =>
    if a.toNat < b.toNat then true else if a = b then tokLe as bs else false

theorem tokLe_total : ∀ a b : Token, tokLe a b = true ∨ tokLe b a = true
  | [], _ => Or.inl rfl
  | _ :: _, [] => Or.inr rfl
  | x :: xs, y :: ys => by
    by_cases h1 : x.toNat < y.toNat
    · left; simp [tokLe, h1]
    · by_cases h2 : y.toNat < x.toNat
      · right; simp [tokLe, h2]
      · have hxy : x = y := Char.toNat_inj.mp (by omega)
        subst hxy
        simp only [tokLe, h1, ite_false, ite_true]
        exact tokLe_total xs ys

theorem tokLe_antisymm : ∀ a b : Token, tokLe a b = true → tokLe b a = true → a = b
  | [], [], _, _ => rfl
  | [], _ :: _, _, h => by simp [tokLe] at h
  | _ :: _, [], h, _ => by simp [tokLe] at h
  | x :: xs, y :: ys, h1, h2 => by
    simp only [tokLe] at h1 h2
    by_cases hxy : x = y
    · subst hxy
      simp at h1 h2
      rw [tokLe_antisymm xs ys h1 h2]
    · have hn : x.toNat ≠ y.toNat := fun h => hxy (Char.toNat_inj.mp h)
      have hyx : ¬ y = x := fun h => hxy h.symm
      simp only [hxy, hyx, ite_false] at h1 h2
      split at h1 <;> split at h2 <;> first | omega | simp_all

theorem tokLe_trans : ∀ a b c : Token, tokLe a b = true → tokLe b c = true → tokLe a c = true
  | [], _, _, _, _ => rfl
  | _ :: _, [], _, h, _ => by simp [tokLe] at h
  | _ :: _, _ :: _, [], _, h => by simp [tokLe] at h
  | x :: xs, y :: ys, z :: zs, h1, h2 => by
    simp only [tokLe] at h1 h2 ⊢
    by_cases hxy : x = y
    · subst hxy
      by_cases hyz : x = z
      · subst hyz
        simp at h1 h2 ⊢
        exact tokLe_trans xs ys zs h1 h2
      · simp only [Nat.lt_irrefl, ite_false, ite_true] at h1
        simp only [hyz, ite_false] at h2 ⊢
        split at h2
        · simp [*]
        · cases h2
    · simp only [hxy, ite_false] at h1
      split at h1
      · by_cases hyz : y = z
        · subst hyz
          simp [*]
        · simp only [hyz, ite_false] at h2
          split at h2
          · have : x.toNat < z.toNat := by omega
            simp [this]
          · cases h2
      · cases h1

/-- Insert into a sorted list. -/
def ins (x : Token) : List Token → List Token
  | [] => [x]
  | y :: ys => if tokLe x y then x :: y :: ys else y :: ins x ys

/-- `stems.sort()`. -/
def sort : List Token → List Token
  | [] => []
  | x :: xs => ins x (sort xs)

theorem ins_comm (x y : Token) : ∀ l : List Token, ins x (ins y l) = ins y (ins x l)
  | [] => by
    simp only [ins]
    cases hxy : tokLe x y <;> cases hyx : tokLe y x
    · have := tokLe_total x y; simp_all
    · simp
    · simp
    · rw [tokLe_antisymm x y hxy hyx]
  | z :: zs => by
    have ih := ins_comm x y zs
    have tot := tokLe_total x y
    cases hxz : tokLe x z <;> cases hyz : tokLe y z
    · simp [ins, hxz, hyz, ih]
    · -- y ≤ z, not x ≤ z: then not x ≤ y
      have hxy : tokLe x y = false := by
        cases h : tokLe x y
        · rfl
        · rw [tokLe_trans x y z h hyz] at hxz; cases hxz
      simp [ins, hxz, hyz, hxy]
    · have hyx : tokLe y x = false := by
        cases h : tokLe y x
        · rfl
        · rw [tokLe_trans y x z h hxz] at hyz; cases hyz
      simp [ins, hxz, hyz, hyx]
    · cases hxy : tokLe x y <;> cases hyx : tokLe y x
      · simp_all
      · simp [ins, hxz, hyz, hxy, hyx]
      · simp [ins, hxz, hyz, hxy, hyx]
      · rw [tokLe_antisymm x y hxy hyx]

theorem ins_perm (x : Token) : ∀ l : List Token, (ins x l).Perm (x :: l)
  | [] => List.Perm.refl _
  | y :: ys => by
    simp only [ins]
    split
    · exact List.Perm.refl _
    · exact ((ins_perm x ys).cons y).trans (List.Perm.swap x y ys)

theorem sort_perm : ∀ l : List Token, (sort l).Perm l
  | [] => List.Perm.refl _
  | x :: xs => (ins_perm x (sort xs)).trans ((sort_perm xs).cons x)

/-- The sorted list depends only on the multiset of its input. -/
theorem sort_eq_of_perm {l₁ l₂ : List Token} (h : l₁.Perm l₂) : sort l₁ = sort l₂ := by
  induction h with
  | nil => rfl
  | cons x _ ih => simp [sort, ih]
  | swap x y l => simp [sort, ins_comm]
  | trans _ _ ih1 ih2 => exact ih1.trans ih2

theorem sort_sort (l : List Token) : sort (sort l) = sort l := sort_eq_of_perm (sort_perm l)

/-! ## The canonical key and `name_key` -/

/-- `canonical_key(content, synonyms)`: trim, one raw synonym lookup, normalize,
sort, join. -/
def canonicalKey (stop : Token → Bool) (stem : Token → Token) (syn : Text → Option Text)
    (s : Text) : Text :=
  let raw := trim s
  joinSp (sort (tokens stop stem ((syn raw).getD raw)))

/-- `canonical_key(name, |_| None)`, the key `name_key` uses. -/
def key (stop : Token → Bool) (stem : Token → Token) (s : Text) : Text :=
  canonicalKey stop stem (fun _ => none) s

/-- `name_key`'s id check: a UUID (`isUuid`, standing for `Uuid::parse_str`)
or exactly 8 ASCII hex digits. Applied to the trimmed name. -/
def idShaped (isUuid : Text → Bool) (t : Text) : Bool :=
  isUuid t || (t.length == 8 && t.all isHex)

/-- `name_key`: `none` is a refusal. `sizeOk` stands for `check_size`. -/
def nameKey (sizeOk isUuid : Text → Bool) (stop : Token → Bool) (stem : Token → Token)
    (s : Text) : Option Text :=
  if sizeOk s = false then none
  else
    let k := key stop stem s
    if k.isEmpty || idShaped isUuid (trim s) then none else some k

theorem key_eq (s : Text) : key stop stem s = joinSp (sort (tokens stop stem s)) := by
  simp [key, canonicalKey, tokens_trim]

/-- A synonym hit in `canonical_key` is normalized like ordinary input, with
no second synonym lookup. -/
theorem canonicalKey_syn {syn : Text → Option Text} {s t : Text} (h : syn (trim s) = some t) :
    canonicalKey stop stem syn s = key stop stem t := by
  simp [canonicalKey, h, key_eq]

/-- Without a synonym hit, `canonical_key` is the plain key of the trimmed
input. -/
theorem canonicalKey_no_syn {syn : Text → Option Text} {s : Text} (h : syn (trim s) = none) :
    canonicalKey stop stem syn s = key stop stem s := by
  simp [canonicalKey, h, key_eq, tokens_trim]

/-! ### Order, separators, case -/

/-- **Token order is ignored.** -/
theorem key_perm {a b : Text} (h : (tokens stop stem a).Perm (tokens stop stem b)) :
    key stop stem a = key stop stem b := by
  rw [key_eq, key_eq, sort_eq_of_perm h]

/-- Swapping the two sides of a separator, with any separators, keeps the key. -/
theorem key_swap {c d : Char} (hc : isSep c = true) (hd : isSep d = true) (a b : Text) :
    key stop stem (a ++ c :: b) = key stop stem (b ++ d :: a) := by
  apply key_perm
  rw [tokens_append_sep hc, tokens_append_sep hd]
  exact List.perm_append_comm

/-- Any reordering of the separated pieces keeps the key. -/
theorem key_segments_perm {c : Char} (hc : isSep c = true) {segs segs' : List Text}
    (h : segs.Perm segs') : key stop stem (joinWith c segs) = key stop stem (joinWith c segs') := by
  apply key_perm
  rw [tokens_joinWith hc, tokens_joinWith hc]
  exact h.flatMap_right _

/-- **`-`, `_` and whitespace are treated alike.** -/
theorem key_sep_alike {c d : Char} (hc : isSep c = true) (hd : isSep d = true) (a b : Text) :
    key stop stem (a ++ c :: b) = key stop stem (a ++ d :: b) := by
  rw [key_eq, key_eq, tokens_append_sep hc, tokens_append_sep hd]

/-- A run of separators counts as one. -/
theorem key_sep_run {c d : Char} (hc : isSep c = true) (hd : isSep d = true) (a b : Text) :
    key stop stem (a ++ c :: d :: b) = key stop stem (a ++ c :: b) := by
  rw [key_eq, key_eq, tokens_append_sep hc, tokens_append_sep hc, tokens_sep_cons hd]

theorem map_lowerA_noUpper (t : Token) (h : ∀ c ∈ t, isUpperA c = false) : t.map lowerA = t := by
  induction t with
  | nil => rfl
  | cons a as ih =>
    simp only [List.map_cons, lowerA_of_not_upper (h a (by simp)),
      ih (fun c hc => h c (by simp [hc]))]

/-- **Case**: where the camelCase split inserts nothing, lowercasing keeps the key. -/
theorem key_lower {s : Text} (h : camel false s = s) :
    key stop stem (s.map lowerA) = key stop stem s := by
  rw [key_eq, key_eq]
  have hl : camel false (s.map lowerA) = s.map lowerA :=
    camel_noUpper false _ (by simp [lowerA_not_upper])
  simp only [tokens, hl, h, words_map lowerA isSep_lowerA, List.map_map, Function.comp_def,
    lowerA_idem]

/-- An all-caps spelling has the canonical key of its lowercase form. -/
theorem key_lower_of_noLower {s : Text} (h : ∀ c ∈ s, isLowerA c = false) :
    key stop stem (s.map lowerA) = key stop stem s :=
  key_lower (camel_noLower s h)

/-! ### Stopwords -/

/-- A single word with no camelCase boundary inside, whose lowercase form is a
stopword, contributes no token to the canonical key. -/
theorem tokens_stopword {w : Text} (hw : Good w) (hc : camel false w = w)
    (hs : stop (w.map lowerA) = true) : tokens stop stem w = [] := by
  simp [tokens, hc, words_single w hw.1 hw.2, hs]

/-- **Stopwords are ignored**: a piece with no tokens can be added on either side
of a separator. -/
theorem key_stop_segment {c : Char} (hc : isSep c = true) {w : Text}
    (hw : tokens stop stem w = []) (s : Text) :
    key stop stem (w ++ c :: s) = key stop stem s ∧ key stop stem (s ++ c :: w) = key stop stem s := by
  constructor
  · rw [key_eq, key_eq, tokens_append_sep hc, hw, List.nil_append]
  · rw [key_eq, key_eq, tokens_append_sep hc, hw, List.append_nil]

/-- An input of only stopwords (and separators) has the empty key. -/
theorem key_all_stop {s : Text}
    (h : ∀ w ∈ words (camel false s), stop (w.map lowerA) = true) : key stop stem s = [] := by
  rw [key_eq]
  have : tokens stop stem s = [] := by
    simp only [tokens, List.map_eq_nil_iff, List.filter_eq_nil_iff, List.mem_map]
    rintro _ ⟨w, hw, rfl⟩
    simp [h w hw]
  rw [this]; rfl

/-! ### Multisets of stems -/

/-- What the stemmer receives: a lowercased, nonempty, separator-free,
non-stopword, non-identifier token. -/
def Called (stop : Token → Bool) (w : Token) : Prop :=
  Good w ∧ (∀ c ∈ w, isUpperA c = false) ∧ stop w = false ∧ isIdent w = false

/-- A stemmer that never returns an empty token or one with a separator in it. -/
structure Sane (stop : Token → Bool) (stem : Token → Token) : Prop where
  ne : ∀ w, Called stop w → stem w ≠ []
  noSep : ∀ w, Called stop w → ∀ c ∈ stem w, isSep c = false

/-- A `Sane` stemmer whose outputs are lowercase, are not stopwords, and are
fixed points of the stemmer. -/
structure Stable (stop : Token → Bool) (stem : Token → Token) : Prop extends Sane stop stem where
  noUpper : ∀ w, Called stop w → ∀ c ∈ stem w, isUpperA c = false
  notStop : ∀ w, Called stop w → stop (stem w) = false
  fix : ∀ w, Called stop w → stem (stem w) = stem w

theorem mem_tokens {s : Text} {t : Token} (h : t ∈ tokens stop stem s) :
    ∃ w ∈ words (camel false s), stop (w.map lowerA) = false ∧
      t = stemOrKeep stem (w.map lowerA) := by
  simp only [tokens, List.mem_map, List.mem_filter, Bool.not_eq_true'] at h
  obtain ⟨_, ⟨⟨w, hw, rfl⟩, hs⟩, rfl⟩ := h
  exact ⟨w, hw, hs, rfl⟩

theorem lowered_word {w : Token} (hw : Good w) :
    Good (w.map lowerA) ∧ ∀ c ∈ w.map lowerA, isUpperA c = false := by
  refine ⟨⟨by simpa using hw.1, ?_⟩, ?_⟩
  · intro c hc
    simp only [List.mem_map] at hc
    obtain ⟨d, hd, rfl⟩ := hc
    rw [isSep_lowerA]; exact hw.2 d hd
  · intro c hc
    simp only [List.mem_map] at hc
    obtain ⟨d, _, rfl⟩ := hc
    exact lowerA_not_upper d

theorem tokens_good (hs : Sane stop stem) {s : Text} : ∀ t ∈ tokens stop stem s, Good t := by
  intro t ht
  obtain ⟨w, hw, hstop, rfl⟩ := mem_tokens ht
  have ⟨hg, hu⟩ := lowered_word (words_good _ w hw)
  unfold stemOrKeep
  split
  · exact hg
  · rename_i hid
    have hc : Called stop (w.map lowerA) := ⟨hg, hu, hstop, by simpa using hid⟩
    exact ⟨hs.ne _ hc, hs.noSep _ hc⟩

/-- A token re-tokenizes to itself (the step behind idempotence). -/
theorem tokens_of_token (hs : Stable stop stem) {s : Text} :
    ∀ t ∈ tokens stop stem s, tokens stop stem t = [t] := by
  intro t ht
  have hgood := tokens_good hs.toSane t ht
  obtain ⟨w, hw, hstop, rfl⟩ := mem_tokens ht
  have ⟨hg, hu⟩ := lowered_word (words_good _ w hw)
  -- the token has no uppercase, is not a stopword, and is kept by `stemOrKeep`
  have key3 : (∀ c ∈ stemOrKeep stem (w.map lowerA), isUpperA c = false) ∧
      stop (stemOrKeep stem (w.map lowerA)) = false ∧
      stemOrKeep stem (stemOrKeep stem (w.map lowerA)) = stemOrKeep stem (w.map lowerA) := by
    unfold stemOrKeep
    by_cases hid : isIdent (w.map lowerA) = true
    · simp only [hid, ite_true]; exact ⟨hu, hstop, by simp⟩
    · have hc : Called stop (w.map lowerA) := ⟨hg, hu, hstop, by simpa using hid⟩
      simp only [hid, Bool.false_eq_true, ite_false]
      refine ⟨hs.noUpper _ hc, hs.notStop _ hc, ?_⟩
      split
      · rfl
      · exact hs.fix _ hc
  obtain ⟨hu', hst', hfix⟩ := key3
  generalize hT : stemOrKeep stem (w.map lowerA) = T at hgood hu' hst' hfix
  simp [tokens, camel_noUpper false T hu', words_single T hgood.1 hgood.2,
    map_lowerA_noUpper T hu', hst', hfix]

/-- **Same key iff same multiset of stems** (for a `Sane` stemmer). -/
theorem key_eq_iff_perm (hs : Sane stop stem) (a b : Text) :
    key stop stem a = key stop stem b ↔ (tokens stop stem a).Perm (tokens stop stem b) := by
  constructor
  · intro h
    rw [key_eq, key_eq] at h
    have hgood : ∀ s, ∀ t ∈ sort (tokens stop stem s), Good t := fun s t ht =>
      tokens_good hs t ((sort_perm _).mem_iff.mp ht)
    have hsort := joinWith_inj (by decide) (hgood a) (hgood b) h
    exact (sort_perm _).symm.trans (hsort ▸ sort_perm _)
  · exact key_perm

/-- With a stemmer that gives every word a nonempty, separator-free stem
(`Sane`, which the real stemmer is not: see `empty_stem_key`), two strings get
the same canonical key (`src/graph/canonical.rs`) exactly when their tokens
agree with multiplicities. -/
theorem key_eq_iff_count (hs : Sane stop stem) (a b : Text) :
    key stop stem a = key stop stem b ↔
      ∀ t, (tokens stop stem a).count t = (tokens stop stem b).count t := by
  rw [key_eq_iff_perm hs, List.perm_iff_count]

/-- **Idempotence** (for a `Stable` stemmer): normalizing a key gives the key. -/
theorem key_idem (hs : Stable stop stem) (s : Text) :
    key stop stem (key stop stem s) = key stop stem s := by
  have hall : ∀ t ∈ sort (tokens stop stem s), tokens stop stem t = [t] := fun t ht =>
    tokens_of_token hs t ((sort_perm _).mem_iff.mp ht)
  have hflat : ∀ L : List Token, (∀ t ∈ L, tokens stop stem t = [t]) →
      L.flatMap (tokens stop stem) = L := by
    intro L hL
    induction L with
    | nil => rfl
    | cons t ts ih =>
      simp only [List.flatMap_cons, hL t (by simp), ih (fun u hu => hL u (by simp [hu]))]
      rfl
  rw [key_eq (key stop stem s), key_eq s, tokens_joinWith (by decide), hflat _ hall, sort_sort]

/-- `Stable` is satisfiable: the identity stemmer, with any stopword list. -/
theorem stable_id (stop : Token → Bool) : Stable stop id where
  ne := fun _ h => h.1.1
  noSep := fun _ h => h.1.2
  noUpper := fun _ h => h.2.1
  notStop := fun _ h => h.2.2.1
  fix := fun _ _ => rfl

/-! ### `name_key` -/

variable {sizeOk isUuid : Text → Bool}

theorem nameKey_empty {s : Text} (h : key stop stem s = []) :
    nameKey sizeOk isUuid stop stem s = none := by
  unfold nameKey
  split
  · rfl
  · simp [h]

/-- When `name_key` gives a key, it is the input's canonical key and it is not
empty. -/
theorem nameKey_some {s k : Text} (h : nameKey sizeOk isUuid stop stem s = some k) :
    k = key stop stem s ∧ k ≠ [] := by
  unfold nameKey at h
  split at h
  · cases h
  · simp only at h
    split at h
    · cases h
    · rename_i hk
      simp only [Option.some.injEq] at h
      subst h
      refine ⟨rfl, fun he => ?_⟩
      simp [he] at hk

theorem nameKey_of_ok {s : Text} (hsz : sizeOk s = true) (hid : idShaped isUuid (trim s) = false) :
    nameKey sizeOk isUuid stop stem s =
      if key stop stem s = [] then none else some (key stop stem s) := by
  unfold nameKey
  simp only [hsz, Bool.true_eq_false, ite_false, hid, Bool.or_false]
  by_cases h : key stop stem s = [] <;> simp [h]

/-- **Names collide exactly when their keys do** (both names well-sized and
not id-shaped). -/
theorem nameKey_eq_iff_key {a b : Text}
    (ha : sizeOk a = true) (hia : idShaped isUuid (trim a) = false)
    (hb : sizeOk b = true) (hib : idShaped isUuid (trim b) = false) :
    nameKey sizeOk isUuid stop stem a = nameKey sizeOk isUuid stop stem b ↔
      key stop stem a = key stop stem b := by
  rw [nameKey_of_ok ha hia, nameKey_of_ok hb hib]
  by_cases h1 : key stop stem a = [] <;> by_cases h2 : key stop stem b = [] <;> simp [h1, h2]
  all_goals (intro h; simp_all)

/-- Names collide exactly when their stems form the same multiset. -/
theorem nameKey_eq_iff_perm (hs : Sane stop stem) {a b : Text}
    (ha : sizeOk a = true) (hia : idShaped isUuid (trim a) = false)
    (hb : sizeOk b = true) (hib : idShaped isUuid (trim b) = false) :
    nameKey sizeOk isUuid stop stem a = nameKey sizeOk isUuid stop stem b ↔
      (tokens stop stem a).Perm (tokens stop stem b) := by
  rw [nameKey_eq_iff_key ha hia hb hib, key_eq_iff_perm hs]

/-- #290: a name with its pieces swapped is the same name. -/
theorem nameKey_swap {c d : Char} (hc : isSep c = true) (hd : isSep d = true) {a b : Text}
    (h1 : sizeOk (a ++ c :: b) = true) (h1' : idShaped isUuid (trim (a ++ c :: b)) = false)
    (h2 : sizeOk (b ++ d :: a) = true) (h2' : idShaped isUuid (trim (b ++ d :: a)) = false) :
    nameKey sizeOk isUuid stop stem (a ++ c :: b) = nameKey sizeOk isUuid stop stem (b ++ d :: a) :=
  (nameKey_eq_iff_key h1 h1' h2 h2').mpr (key_swap hc hd a b)

/-- #290: a stopword piece does not distinguish names. -/
theorem nameKey_stop_segment {c : Char} (hc : isSep c = true) {w s : Text}
    (hw : tokens stop stem w = [])
    (h1 : sizeOk (w ++ c :: s) = true) (h1' : idShaped isUuid (trim (w ++ c :: s)) = false)
    (h2 : sizeOk s = true) (h2' : idShaped isUuid (trim s) = false) :
    nameKey sizeOk isUuid stop stem (w ++ c :: s) = nameKey sizeOk isUuid stop stem s :=
  (nameKey_eq_iff_key h1 h1' h2 h2').mpr (key_stop_segment hc hw s).1

/-- A stopword-only name is refused. -/
theorem nameKey_all_stop {s : Text}
    (h : ∀ w ∈ words (camel false s), stop (w.map lowerA) = true) :
    nameKey sizeOk isUuid stop stem s = none :=
  nameKey_empty (key_all_stop h)

/-! ## The fix for #301, item 1: empty stems are dropped

`normalize_tokens` after the fix drops every empty stem
(`.filter(|t| !t.is_empty())` after stemming). `tokens`, `key` and `nameKey`
above stay as they are and describe the previous version; `tokensF`, `keyF`
and `nameKeyF` are the fixed ones. The fix needs only that the stemmer never
puts a separator inside a stem (`NoSep`, the half of `Sane` the real stemmer
meets); the other half, nonempty stems, is what the filter now provides. -/

/-- A stemmer that never returns a stem with a separator in it. -/
def NoSep (stop : Token → Bool) (stem : Token → Token) : Prop :=
  ∀ w, Called stop w → ∀ c ∈ stem w, isSep c = false

/-- `.filter(|t| !t.is_empty())`. -/
def dropEmpty (ts : List Token) : List Token := ts.filter (fun t => !t.isEmpty)

/-- `normalize_tokens` after the fix. -/
def tokensF (stop : Token → Bool) (stem : Token → Token) (s : Text) : List Token :=
  dropEmpty (tokens stop stem s)

/-- `canonical_key` after the fix. -/
def canonicalKeyF (stop : Token → Bool) (stem : Token → Token) (syn : Text → Option Text)
    (s : Text) : Text :=
  let raw := trim s
  joinSp (sort (tokensF stop stem ((syn raw).getD raw)))

/-- `canonical_key(name, |_| None)` after the fix. -/
def keyF (stop : Token → Bool) (stem : Token → Token) (s : Text) : Text :=
  canonicalKeyF stop stem (fun _ => none) s

/-- `name_key` after the fix: the same checks, on the fixed key. -/
def nameKeyF (sizeOk isUuid : Text → Bool) (stop : Token → Bool) (stem : Token → Token)
    (s : Text) : Option Text :=
  if sizeOk s = false then none
  else
    let k := keyF stop stem s
    if k.isEmpty || idShaped isUuid (trim s) then none else some k

/-- No two spaces in a row. -/
def noDoubleSp : Text → Bool
  | a :: b :: cs => !(a == ' ' && b == ' ') && noDoubleSp (b :: cs)
  | _ => true

/-- No leading, trailing or doubled space. -/
def tidy (k : Text) : Bool :=
  k.head? != some ' ' && k.getLast? != some ' ' && noDoubleSp k

theorem keyF_eq (s : Text) : keyF stop stem s = joinSp (sort (tokensF stop stem s)) := by
  simp [keyF, canonicalKeyF, tokensF, tokens_trim]

theorem mem_tokensF {s : Text} {t : Token} :
    t ∈ tokensF stop stem s ↔ t ∈ tokens stop stem s ∧ t ≠ [] := by
  simp [tokensF, dropEmpty]

/-- A token of the old tokenizer has no separator in it (for a `NoSep` stemmer). -/
theorem tokens_noSep (hn : NoSep stop stem) {s : Text} :
    ∀ t ∈ tokens stop stem s, ∀ c ∈ t, isSep c = false := by
  intro t ht
  obtain ⟨w, hw, hstop, rfl⟩ := mem_tokens ht
  have ⟨hg, hu⟩ := lowered_word (words_good _ w hw)
  unfold stemOrKeep
  split
  · exact hg.2
  · rename_i hid
    exact hn _ ⟨hg, hu, hstop, by simpa using hid⟩

/-- Every fixed token is `Good`: nonempty and separator-free. -/
theorem tokensF_good (hn : NoSep stop stem) {s : Text} : ∀ t ∈ tokensF stop stem s, Good t := by
  intro t ht
  have ⟨h1, h2⟩ := mem_tokensF.mp ht
  exact ⟨h2, tokens_noSep hn t h1⟩

theorem sp_isSep : isSep ' ' = true := by decide

theorem ne_sp_of_good {t : Token} (ht : Good t) : ∀ x ∈ t, x ≠ ' ' := by
  intro x hx h
  subst h
  have := ht.2 _ hx
  rw [sp_isSep] at this
  cases this

theorem noDoubleSp_cons {x : Char} (hx : x ≠ ' ') (l : Text) :
    noDoubleSp (x :: l) = noDoubleSp l := by
  cases l with
  | nil => rfl
  | cons y ys => simp [noDoubleSp, hx]

theorem noDoubleSp_noSp : ∀ t : Text, (∀ x ∈ t, x ≠ ' ') → noDoubleSp t = true
  | [], _ => rfl
  | x :: xs, h => by
    rw [noDoubleSp_cons (h x (by simp))]
    exact noDoubleSp_noSp xs (fun y hy => h y (by simp [hy]))

theorem noDoubleSp_append_sp {R : Text} (hR : R.head? ≠ some ' ') (hR2 : noDoubleSp R = true) :
    ∀ X : Text, (∀ x ∈ X, x ≠ ' ') → noDoubleSp (X ++ ' ' :: R) = true
  | [], _ => by
    cases R with
    | nil => rfl
    | cons r rs =>
      have hr : r ≠ ' ' := fun h => hR (by simp [h])
      simp [noDoubleSp, hr, hR2]
  | x :: xs, h => by
    rw [List.cons_append, noDoubleSp_cons (h x (by simp))]
    exact noDoubleSp_append_sp hR hR2 xs (fun y hy => h y (by simp [hy]))

theorem joinSp_head : ∀ L : List Token, (∀ t ∈ L, Good t) →
    ∀ x, (joinSp L).head? = some x → isSep x = false
  | [], _, x, h => by simp [joinWith] at h
  | [t], hL, x, h => by
    have ht := hL t (by simp)
    simp only [joinWith] at h
    exact ht.2 x (List.mem_of_mem_head? h)
  | t :: u :: us, hL, x, h => by
    have ht := hL t (by simp)
    simp only [joinWith] at h
    obtain ⟨a, as, rfl⟩ := List.exists_cons_of_ne_nil ht.1
    simp only [List.cons_append, List.head?_cons, Option.some.injEq] at h
    subst h
    exact ht.2 _ (by simp)

theorem joinSp_ne_nil : ∀ L : List Token, (∀ t ∈ L, Good t) → L ≠ [] → joinSp L ≠ []
  | [], _, h => absurd rfl h
  | [t], hL, _ => by simpa [joinWith] using (hL t (by simp)).1
  | t :: u :: us, _, _ => by simp [joinWith]

theorem joinSp_last : ∀ L : List Token, (∀ t ∈ L, Good t) →
    ∀ x, (joinSp L).getLast? = some x → isSep x = false
  | [], _, x, h => by simp [joinWith] at h
  | [t], hL, x, h => by
    have ht := hL t (by simp)
    simp only [joinWith] at h
    exact ht.2 x (List.mem_of_getLast? h)
  | t :: u :: us, hL, x, h => by
    have hR := joinSp_ne_nil (u :: us) (fun v hv => hL v (by simp_all)) (by simp)
    have ih := joinSp_last (u :: us) (fun v hv => hL v (by simp_all))
    change (t ++ ' ' :: joinSp (u :: us)).getLast? = some x at h
    generalize joinSp (u :: us) = R at h hR ih
    obtain ⟨r, rs, rfl⟩ := List.exists_cons_of_ne_nil hR
    rw [List.getLast?_append, List.getLast?_cons_cons] at h
    cases hl : (r :: rs).getLast? with
    | none => simp at hl
    | some y =>
      rw [hl] at h
      simp only [Option.some_or, Option.some.injEq] at h
      subst h
      exact ih _ hl

/-- The space-joined text of `Good` tokens has no leading, trailing or doubled
space. -/
theorem joinSp_tidy : ∀ L : List Token, (∀ t ∈ L, Good t) → tidy (joinSp L) = true := by
  intro L hL
  have hhead : (joinSp L).head? ≠ some ' ' := fun h => by
    have := joinSp_head L hL ' ' h; rw [sp_isSep] at this; cases this
  have hlast : (joinSp L).getLast? ≠ some ' ' := fun h => by
    have := joinSp_last L hL ' ' h; rw [sp_isSep] at this; cases this
  have hdouble : ∀ L : List Token, (∀ t ∈ L, Good t) → noDoubleSp (joinSp L) = true := by
    intro L
    induction L with
    | nil => intro _; rfl
    | cons t ts ih =>
      intro hL
      cases ts with
      | nil => exact noDoubleSp_noSp t (ne_sp_of_good (hL t (by simp)))
      | cons u us =>
        have hR := fun v (hv : v ∈ u :: us) => hL v (by simp_all)
        have hRh : (joinSp (u :: us)).head? ≠ some ' ' := fun h => by
          have := joinSp_head (u :: us) hR ' ' h; rw [sp_isSep] at this; cases this
        exact noDoubleSp_append_sp hRh (ih hR) t (ne_sp_of_good (hL t (by simp)))
  simp [tidy, hhead, hlast, hdouble L hL]

/-- **The fixed key never has a leading, trailing or doubled space**, whatever
the synonym table, for any stemmer that keeps separators out of its stems. -/
theorem canonicalKeyF_tidy (hn : NoSep stop stem) (syn : Text → Option Text) (s : Text) :
    tidy (canonicalKeyF stop stem syn s) = true :=
  joinSp_tidy _ (fun t ht => tokensF_good hn t ((sort_perm _).mem_iff.mp ht))

theorem keyF_tidy (hn : NoSep stop stem) (s : Text) : tidy (keyF stop stem s) = true :=
  canonicalKeyF_tidy hn _ s

/-- **Agreement**: where no stem is empty, the fixed tokens are the old ones. -/
theorem tokensF_eq_tokens {s : Text} (h : ∀ t ∈ tokens stop stem s, t ≠ []) :
    tokensF stop stem s = tokens stop stem s := by
  simp only [tokensF, dropEmpty]
  rw [List.filter_eq_self]
  intro t ht
  simpa [List.isEmpty_iff] using h t ht

/-- **Agreement**: where no stem is empty, the fixed key is the old key. So
only a key with an empty stem changes. -/
theorem keyF_eq_key {s : Text} (h : ∀ t ∈ tokens stop stem s, t ≠ []) :
    keyF stop stem s = key stop stem s := by
  rw [keyF_eq, key_eq, tokensF_eq_tokens h]

/-- Whenever no token stems to empty, the Rust's `canonical_key` (which drops
empty stems) equals the model's key, synonyms included. -/
theorem canonicalKeyF_eq_canonicalKey {syn : Text → Option Text} {s : Text}
    (h : ∀ t ∈ tokens stop stem ((syn (trim s)).getD (trim s)), t ≠ []) :
    canonicalKeyF stop stem syn s = canonicalKey stop stem syn s := by
  simp only [canonicalKeyF, canonicalKey, tokensF_eq_tokens h]

/-- With a stemmer that never yields an empty stem, the Rust's empty-stem
filter changes no canonical key. -/
theorem keyF_eq_key_of_sane (hs : Sane stop stem) (s : Text) :
    keyF stop stem s = key stop stem s :=
  keyF_eq_key (fun t ht => (tokens_good hs t ht).1)

/-- Whenever no token stems to empty, the Rust's `name_key` (which drops empty
stems) equals the model's. -/
theorem nameKeyF_eq_nameKey {s : Text} (h : ∀ t ∈ tokens stop stem s, t ≠ []) :
    nameKeyF sizeOk isUuid stop stem s = nameKey sizeOk isUuid stop stem s := by
  simp only [nameKeyF, nameKey, keyF_eq_key h]

theorem nameKeyF_empty {s : Text} (h : keyF stop stem s = []) :
    nameKeyF sizeOk isUuid stop stem s = none := by
  unfold nameKeyF
  split
  · rfl
  · simp [h]

/-- **A name whose stems are all empty keys to the empty key**, and the
existing empty-key check refuses it. -/
theorem keyF_all_empty {s : Text} (h : ∀ t ∈ tokens stop stem s, t = []) :
    keyF stop stem s = [] := by
  have : tokensF stop stem s = [] := by
    rw [List.eq_nil_iff_forall_not_mem]
    intro t ht
    have ⟨h1, h2⟩ := mem_tokensF.mp ht
    exact h2 (h t h1)
  rw [keyF_eq, this]; rfl

/-- `name_key` gives no key when every token stems to empty. -/
theorem nameKeyF_all_empty {s : Text} (h : ∀ t ∈ tokens stop stem s, t = []) :
    nameKeyF sizeOk isUuid stop stem s = none :=
  nameKeyF_empty (keyF_all_empty h)

/-- With empty stems gone, the multiset characterization needs only `NoSep`:
two inputs share a fixed key exactly when their nonempty stems form the same
multiset. -/
theorem keyF_eq_iff_perm (hn : NoSep stop stem) (a b : Text) :
    keyF stop stem a = keyF stop stem b ↔ (tokensF stop stem a).Perm (tokensF stop stem b) := by
  constructor
  · intro h
    rw [keyF_eq, keyF_eq] at h
    have hgood : ∀ s, ∀ t ∈ sort (tokensF stop stem s), Good t := fun s t ht =>
      tokensF_good hn t ((sort_perm _).mem_iff.mp ht)
    have hsort := joinWith_inj (by decide) (hgood a) (hgood b) h
    exact (sort_perm _).symm.trans (hsort ▸ sort_perm _)
  · intro h
    rw [keyF_eq, keyF_eq, sort_eq_of_perm h]

/-! ## Concrete examples

`stop0` is `STOPWORDS` from `src/graph/canonical.rs`. `stem0` agrees with the
Snowball English (Porter2) stemmer on the words below. The pairs `user`,
`users`, `schema`, `register`, `create` and `account` are in the Rust test
table `STEM`; the rest follow the Porter2 rules by hand, and every collision and
non-collision stated below was checked against a local debug build of
`lambo derive` (a match against the earlier concept, or a new one). -/

/-- `STOPWORDS`.
Pinned by `formal_constants_tests::canonical`. -/
def stopwords : List String :=
  ["the", "a", "an", "for", "of", "at", "in", "to", "on", "and", "or", "is", "are"]

def stop0 (t : Token) : Bool := stopwords.any (fun w => w.toList == t)

def stemTable : List (String × String) :=
  [("single", "singl"), ("singles", "singl"), ("lease", "leas"), ("leases", "leas"),
   ("leasing", "leas"), ("writer", "writer"), ("writers", "writer"), ("ons", "on"),
   ("onning", "on"), ("user", "user"), ("users", "user"), ("schema", "schema"),
   ("register", "regist"), ("create", "creat"), ("account", "account"),
   ("xmlparser", "xmlparser"), ("xml", "xml"), ("parser", "parser"),
   ("singlewriterlease", "singlewriterleas"), ("leas", "lea"), ("''s", "")]

def stem0 (t : Token) : Token :=
  match stemTable.find? (fun p => p.1.toList == t) with
  | some p => p.2.toList
  | none => t

/-- The key of a string, with the concrete tables. -/
def key0 (s : String) : Text := key stop0 stem0 s.toList

/-- `name_key` with the concrete tables, for names that pass `check_size` and
are not UUIDs. -/
def nameKey0 (s : String) : Option Text := nameKey (fun _ => true) (fun _ => false) stop0 stem0 s.toList

-- The Rust tests `canonical_key_sorts_and_joins` and `normalize_*`.
example : key0 "User Schema" = "schema user".toList := by decide
example : key0 "UserSchema" = "schema user".toList := by decide
example : key0 "user_schema" = "schema user".toList := by decide
example : key0 "user-_schema" = "schema user".toList := by decide
example : key0 "schema the user" = "schema user".toList := by decide
example : key0 "register user" = "regist user".toList := by decide
example : key0 "create account" = "account creat".toList := by decide
example : key0 "the and of" = [] := by decide
example : key0 "  " = [] := by decide
example : key0 "-_" = [] := by decide

/-- #290, item 2: word order, case, camel case and plurals do not distinguish
names. All three spellings are the name `leas singl writer`. -/
theorem issue290_order :
    nameKey0 "single-writer-lease" = some "leas singl writer".toList ∧
    nameKey0 "lease writer single" = some "leas singl writer".toList ∧
    nameKey0 "SingleWriterLeases" = some "leas singl writer".toList := by decide

/-- #290, item 2: a stopword does not distinguish names. -/
theorem issue290_stopword : nameKey0 "the-lease" = nameKey0 "lease" ∧
    nameKey0 "lease" = some "leas".toList := by decide

/-- More collisions of the same kind. -/
example : nameKey0 "Single-Writer-Lease" = nameKey0 "single-writer-lease" := by decide
example : nameKey0 "SINGLE_WRITER_LEASE" = nameKey0 "single-writer-lease" := by decide
example : nameKey0 "single  writer\tlease" = nameKey0 "single-writer-lease" := by decide
example : nameKey0 "the-lease-of-the-writer" = nameKey0 "writer-lease" := by decide
example : nameKey0 "singles-writers" = nameKey0 "single-writer" := by decide
example : nameKey0 "leasing" = nameKey0 "lease" := by decide

/-- Case is not ignored across a camelCase boundary: `SingleWriterLease` is
three words, `singlewriterlease` one. -/
theorem camel_not_lower : key0 "SingleWriterLease" ≠ key0 "singlewriterlease" := by decide

/-- The camelCase split is ASCII lower-to-upper only: an acronym run or a digit
does not split. -/
example : key0 "XMLParser" ≠ key0 "xml-parser" := by decide
example : key0 "xmlParser" = key0 "xml-parser" := by decide
example : key0 "v2Lease" ≠ key0 "v2-lease" := by decide
example : key0 "leases-v2" = key0 "v2 lease" := by decide

example : key0 "XmlParser" = key0 "xml-parser" := by decide

/-- `.` and `/` are not separators: once split at separators and camelCase
boundaries, a piece containing them (or a digit) is one identifier token
(#148), lowercased and not stemmed. Without a camelCase boundary a dotted or
slashed piece is therefore neither reordered inside nor stripped of
stopwords, but its case is ignored. -/
example : key0 "single.writer.lease" ≠ key0 "single-writer-lease" := by decide
example : key0 "leases/v2" ≠ key0 "v2/leases" := by decide
example : key0 "Leases.V2" = key0 "leases.v2" := by decide

/-- The camelCase split runs before the identifier rule, so a lower-to-upper
boundary inside a dotted or slashed piece still splits and reorders it. -/
example : key0 "src/MyFile.rs" = key0 "File.rs src/My" := by decide
example : key0 "src/MyFile.rs" = "file.rs src/my".toList := by decide
example : key0 "leaseV2" = key0 "v2-lease" := by decide

/-- The canonical key is a multiset: a repeated word is not collapsed (`lease
lease` and `lease` differ). -/
theorem dup_not_collapsed : key0 "lease lease" ≠ key0 "lease" := by decide

/-- Stopword-only and id-shaped names are refused. -/
example : nameKey0 "the" = none := by decide
example : nameKey0 "the-of" = none := by decide
example : nameKey0 "DeadBeef" = none := by decide

/-- **The real key is not idempotent.** Snowball English stems `lease` to
`leas` and `leas` to `lea` (`Stable.fix` fails on an ordinary word), so the
key of `lease` is `leas` and normalizing it again gives `lea`. A displayed key
typed back as a name is a different name. -/
theorem leas_not_idempotent :
    key0 "lease" = "leas".toList ∧ key0 "leas" = "lea".toList ∧
    key stop0 stem0 (key0 "lease") ≠ key0 "lease" ∧
    nameKey0 "leas singl writer" = some "lea singl writer".toList ∧
    nameKey0 "leas singl writer" ≠ nameKey0 "single-writer-lease" := by decide

/-- A second, rarer failure of `Stable`: `ons` stems to the stopword `on`, so
the key of `ons` is `on`, whose own key is empty. Normalizing that key again
is not the identity, and the name `ons` is accepted while `on` is refused. -/
theorem ons_not_idempotent :
    key0 "ons" = "on".toList ∧ key stop0 stem0 (key0 "ons") = [] ∧
    nameKey0 "ons" = some "on".toList ∧ nameKey0 "on" = none := by decide

/-- The real stemmer is not `Sane`: `''s` stems to the empty string, and
`tokens` does not drop empty stems. `''s` alone has the empty key (refused as
a name), but `''s-lease` keys as ` leas` with a leading space, and `''s ''s`
as a single space, which `name_key` accepts. -/
theorem empty_stem_key :
    key0 "''s" = [] ∧ nameKey0 "''s" = none ∧
    key0 "''s-lease" = " leas".toList ∧ nameKey0 "''s-lease" = some " leas".toList ∧
    nameKey0 "''s ''s" = some " ".toList := by decide

/-! ### After the fix for #301 -/

/-- The fixed key with the concrete tables. -/
def keyF0 (s : String) : Text := keyF stop0 stem0 s.toList

/-- The fixed `name_key` with the concrete tables. -/
def nameKeyF0 (s : String) : Option Text :=
  nameKeyF (fun _ => true) (fun _ => false) stop0 stem0 s.toList

theorem stemTable_noSep : ∀ p ∈ stemTable, ∀ c ∈ p.2.toList, isSep c = false := by decide

/-- `stem0` keeps separators out of its stems, though it is not `Sane`, so the
fixed-key theorems apply to it. -/
theorem noSep_stem0 : NoSep stop0 stem0 := by
  intro w hw c hc
  unfold stem0 at hc
  split at hc
  · rename_i p hp
    exact stemTable_noSep p (List.mem_of_find?_eq_some hp) c hc
  · exact hw.1.2 c hc

/-- Every fixed key with the concrete tables is tidy. -/
theorem keyF0_tidy (s : String) : tidy (keyF0 s) = true := keyF_tidy noSep_stem0 _

/-- **#301, item 1, fixed**: `''s-lease` keys as `leas`, the name `lease`,
and `''s ''s` keys as the empty key and is refused. The old keys were not
tidy; the new ones are. -/
theorem issue301_fixed :
    keyF0 "''s-lease" = "leas".toList ∧ nameKeyF0 "''s-lease" = nameKeyF0 "lease" ∧
    nameKeyF0 "''s-lease" = some "leas".toList ∧
    keyF0 "''s ''s" = [] ∧ nameKeyF0 "''s ''s" = none ∧
    tidy (key0 "''s-lease") = false ∧ tidy (key0 "''s ''s") = false ∧
    tidy (keyF0 "''s-lease") = true := by decide

/-- Inputs without an empty stem key as before. -/
example : keyF0 "single-writer-lease" = key0 "single-writer-lease" := by decide
example : nameKeyF0 "SingleWriterLeases" = nameKey0 "SingleWriterLeases" := by decide
example : keyF0 "''s" = key0 "''s" := by decide

/-- The identity stemmer is `Stable`, so `key_idem` applies to it, here on a
non-trivial input. -/
example : key stop0 id (key stop0 id "The-Lease-writer".toList) = key stop0 id "The-Lease-writer".toList :=
  key_idem (stable_id stop0) _


/-- The PostgreSQL unique index stores the session ID and a SHA-256 digest of
the full canonical key. SHA-256 is an opaque function with a 32-byte output.
The full text key is stored separately for canonical matching. -/
def pgIndexKey (digest : List Char → Fin 32 → UInt8) (session key : List Char) :
    List Char × List UInt8 :=
  (session, List.ofFn (digest key))

/-- The canonical key contributes 32 bytes to the PostgreSQL index value,
regardless of its text length. This excludes index row metadata and the session ID. -/
theorem pgIndexKey_width (digest : List Char → Fin 32 → UInt8) (session key : List Char) :
    (pgIndexKey digest session key).2.length = 32 := by
  simp [pgIndexKey]

/-- Copies of a full key conflict in the same session and remain independent
across sessions. No injectivity of the digest is assumed. -/
theorem pgIndexKey_same_key (digest : List Char → Fin 32 → UInt8) (session other key : List Char) :
    pgIndexKey digest session key = pgIndexKey digest other key ↔ session = other := by
  simp [pgIndexKey]

end Lambo.Canonical
