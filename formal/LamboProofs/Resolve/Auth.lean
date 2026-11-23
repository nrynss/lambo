import LamboProofs.Resolve.Serve

/-
# `--auth-token` redaction (`src/main.rs`, #315)

Models the token rule (`SecretToken::new`, `secretNew_ok`), `AuthTokenArg`,
`AuthTokenArgParser`, `http_auth_token_flag` and the `Debug` of
`AuthTokenArg` / `SecretToken` (`parseAuthArg`, `render` over the printing
paths `PrintPath`); `lambo serve-web --auth-token`'s clap error before #315's
fix through `AuthToken: FromStr` (`serveWebFlagRefusal`, the previous version)
and with the fix through `WebAuthTokenParser` and the shared
`auth_token_refusal` (`parseWebAuthArg`, `serveWebFlagRefusalFixed`); and
clap's unexpected-argument error for an argv element after either
`--auth-token`: before the fix (`flagTakesPrev`, `unexpectedArgRefusal`), with
`allow_hyphen_values` and the first `redact_auth_token_echo` in a26ffd30
(`flagTakesHyphen`, `unexpectedArgRefusalFixed`, `argShown`), and since the
re-review without `allow_hyphen_values` and with the combined rule
(`flagTakes`, `authFlagValue`, `quotedRender`, `argShownFixed`). This is #273's
`AuthTokenArg` redaction constraint.

Results: clap's value-quoting error cannot occur for `lambo serve
--auth-token` (`parseAuthArg_total`); on each token printing path the output
depends on the token only through its verdict class (`render_noninterference`)
and the `Debug` paths print the same for every token (`debug_constant`); this
holds by definition of `render`, so its strength is the completeness of
`PrintPath`, a reading of the code. `check_serve_credentials`'s legacy-token
refusal sits outside it: its output depends on whether the token equals a
configured credential's (`credential_refusal_reveals_equality`, the bytes are
not printed). Before #315, `serve-web`'s refusal quoted the value
(`serve_web_flag_refusal_leaks`); the fixed parser refuses the same tokens,
hands on the same token and names the same rule in `serve`'s words, never the
value (`parseWebAuthArg_ok_iff`, `parseWebAuthArg_error_iff`,
`web_flag_refusal_keeps_reason`, `web_flag_refusal_eq_serve`,
`serve_web_flag_refusal_fixed`). Clap's unexpected-argument error quoted a
token starting with `-` and the tail of a token split by an unquoted space
(`hyphen_token_leaked`, `unexpected_arg_leaked`); a26ffd30 took every element
after the flag as its value, which was fail open (`flagTakesHyphen_total`,
`b1_fail_open`); now a spaced `--auth-token` refuses a flag-like element
(`authFlagValue_spaced_none_iff`, `flagTakes_agrees_base`),
`--auth-token=-x` takes any value (`authFlagValue_eq_form`), and every quoting
error prints the same for every element from the flag on
(`quotedRender_noninterference`, `argShownFixed_from_flag`,
`quotedRender_unexpected_is_render`), except a missing value
(`missing_value_kept`) and the flag's own spelling (`re_review_fixed` on the
probes).

Boundary: clap's help and error rendering are not modelled beyond the quoting
paths `Quote` lists, nor redaction anywhere but `serve` and `serve-web`. A
token split by an unquoted space still reveals that it was split and may change
the error, and a piece an accepted flag takes may be printed as that flag's
value (`--transport <piece>` prints "unknown transport" with it, and a
`--session <piece>` appears in `serve-web`'s banner). The router must keep
parsing through `redact_auth_token_echo` for these results to apply.
-/

namespace Lambo.Resolve
/-! ## `--auth-token`: the token check and redaction -/

/-- `SecretToken::new` accepts a token only when it is valid UTF-8, non-empty,
printable, at most `MAX_BEARER` bytes and has no leading or trailing space. -/
theorem secretNew_ok {r : TokenRaw} {k : List Nat} (h : secretNew r = .ok k) :
    k = r.bytes ∧ r.utf8 = true ∧ r.bytes ≠ [] ∧ r.bytes.all printable = true ∧
      r.bytes.length ≤ maxBearer ∧ r.bytes.head? ≠ some 32 ∧ r.bytes.getLast? ≠ some 32 := by
  unfold secretNew at h
  split at h
  · cases h
  · rename_i hu
    split at h
    · cases h
    · rename_i hw
      split at h
      · cases h
      · rename_i hs
        split at h
        · cases h
        · rename_i hp
          split at h
          · cases h
          · rename_i hl
            cases h
            refine ⟨rfl, by simpa using hu, ?_, by simpa using hp, by omega, ?_, ?_⟩
            · rintro he; simp [he] at hw
            · intro h1; simp [h1, isAsciiWs] at hs
            · intro h1; simp [h1, isAsciiWs] at hs

set_option maxRecDepth 20000 in
/-- The token rule on examples: the empty token (an exported but empty
variable), surrounding whitespace, a control byte, a non-UTF-8 value and an
over-long token are refused; an inner space is presentable. -/
theorem secretNew_examples :
    secretNew ⟨[], true⟩ = .error .empty ∧
      secretNew ⟨[32, 97], true⟩ = .error .surroundingSpace ∧
      secretNew ⟨[97, 9, 98], true⟩ = .error .notPrintable ∧
      secretNew ⟨[97], false⟩ = .error .notUtf8 ∧
      secretNew ⟨List.replicate 4097 97, true⟩ = .error .tooLong ∧
      secretNew ⟨[97, 32, 98], true⟩ = .ok [97, 32, 98] := by decide

/-- `AuthTokenArgParser::parse_ref` takes the value as given and never
refuses, so clap's own "invalid value '<it>'" error, which would quote the
token, cannot occur for `--auth-token`. -/
def parseAuthArg (r : TokenRaw) : Except Unit TokenRaw := .ok r

/-- Parsing `--auth-token` (`AuthTokenArg`) accepts every raw value; the
token's checks run later, in `SecretToken::new`. -/
theorem parseAuthArg_total (r : TokenRaw) : parseAuthArg r = .ok r := rfl

def whyText : Why → String
  | .notUtf8 => "auth token is not valid UTF-8"
  | .empty => "auth token is empty — pass a non-empty secret, or omit it entirely to run unauthenticated on loopback"
  | .surroundingSpace => "auth token has leading or trailing whitespace, which a request cannot carry (the presented credential is trimmed); remove it"
  | .notPrintable => "auth token contains a character outside printable ASCII, which an HTTP Authorization header cannot carry"
  | .tooLong => "auth token is longer than 4096 bytes, so no request could present it"

/-- The token's verdict class: `none` for a valid token, else why not. -/
def verdict (r : TokenRaw) : Option Why :=
  match secretNew r with
  | .ok _ => none
  | .error w => some w

/-- `WebAuthTokenParser::parse_ref`, `lambo serve-web --auth-token`'s parser
since #315's fix. Like `AuthToken: FromStr` before it, it checks the token
when clap parses it (UTF-8, then `AuthToken::new`) and refuses exactly the
tokens `secretNew` refuses; unlike it, it hands clap an error it built, so
clap never adds the value. Its refusal is the `Why` alone. -/
def parseWebAuthArg (r : TokenRaw) : Except Why TokenRaw :=
  match verdict r with
  | some w => .error w
  | none => .ok r

/-- `lambo serve-web --auth-token`'s clap error since #315's fix: built by
`auth_token_refusal`, the builder `lambo serve`'s flag refusal uses, so it
names the flag and the rule and never the value. It is still a clap
`ValueValidation` error from the parser, the kind the `FromStr` parser gave,
so the exit code (2) is unchanged. The previous version is
`serveWebFlagRefusal`. -/
def serveWebFlagRefusalFixed (r : TokenRaw) : String :=
  match parseWebAuthArg r with
  | .error w => "invalid value for '--auth-token <TOKEN>': " ++ whyText w ++ " (the value is not shown)"
  | .ok _ => ""

/-- The previous version (before #315's review fix): whether clap took the
argv element after `--auth-token` as its value. Without
`allow_hyphen_values` an element starting with `-` (45) was parsed as a flag
instead and reached clap's unexpected-argument error. -/
def flagTakesPrev (r : TokenRaw) : Bool := r.bytes.head? != some 45

/-- The previous version a26ffd30: with `allow_hyphen_values` on both
`--auth-token` args, the element after the flag was always its value, so a
flag there silently became the token (fail open, #315). -/
def flagTakesHyphen (_r : TokenRaw) : Bool := true

/-- The previous version of clap's unexpected-argument error for an argv
element `a` it could not place, such as the tail of an unquoted token split
by a space (`--auth-token NOTASECRET-a NOTASECRET-extra`): it quotes `a`.
Bytes are read as code points (ASCII here). -/
def unexpectedArgRefusal (a : TokenRaw) : String :=
  "unexpected argument '" ++ String.ofList (a.bytes.map Char.ofNat) ++ "' found"

/-- The tip a26ffd30's `redact_auth_token_echo` put in place of clap's
suggestions (previous version; now `redactTip`). -/
def unexpectedArgTip : String :=
  "an argument after --auth-token may be part of the token, so it is not shown; quote a token that contains a space"

/-- The previous version a26ffd30 of the redacted error, for an element `a`
at or after `--auth-token` in argv: `redact_auth_token_echo` replaced the
quoted element by `<redacted>` and clap's suggestions by one fixed tip. It
kept clap's usage line, which could name clap's did-you-mean flag for the
redacted element (`--porr` gave `--port <PORT>`), so it hinted at it; the
current version is `redactedText`. -/
def unexpectedArgRefusalFixed (_a : TokenRaw) : String :=
  "unexpected argument '<redacted>' found\n\n  tip: " ++ unexpectedArgTip

/-- Code-point infix: `q` occurs in `a` (`str::contains`). -/
def infixOf (q a : List Nat) : Bool :=
  (List.range (a.length + 1)).any fun i => (a.drop i).take q.length == q

/-- `redact_auth_token_echo`'s narrow rule: an element clap quotes (`q`) is
shown only when it occurs in an argv element before the first
`--auth-token` and in none from that flag on (`fromFlag`). -/
def argShown (before fromFlag : List (List Nat)) (q : List Nat) : Bool :=
  before.any (infixOf q) && !fromFlag.any (infixOf q)

/-- Whether clap reads an argv element as a flag rather than a value (no
`allow_hyphen_values`, no negative numbers): it starts with `-` and is not
`-` alone. -/
def looksLikeFlag (r : TokenRaw) : Bool := r.bytes.head? == some 45 && 1 < r.bytes.length

/-- Since #315's re-review: `allow_hyphen_values` is gone again, so the
element after a spaced `--auth-token` is its value only when it does not look
like a flag. -/
def flagTakes (r : TokenRaw) : Bool := !looksLikeFlag r

/-- The value `--auth-token` gets from the next element: `eqForm` is
`--auth-token=<v>`, where clap takes `v` whatever it starts with; `none` is
clap's refusal ("a value is required", or the element is an unexpected
argument), exit 2 at parse time, so nothing starts. -/
def authFlagValue (eqForm : Bool) (next : TokenRaw) : Option TokenRaw :=
  if eqForm || flagTakes next then some next else none

/-- The previous version a26ffd30 of `authFlagValue`: every element was the
value (`flagTakesHyphen`). -/
def authFlagValueHyphen (_eqForm : Bool) (next : TokenRaw) : Option TokenRaw := some next

/-- `--auth-token`, the one spelling `redact_auth_token_echo` never redacts:
it is the flag, not a value (`lambo --auth-token x serve-web` quotes it). -/
def authFlagBytes : List Nat := [45, 45, 97, 117, 116, 104, 45, 116, 111, 107, 101, 110]

/-- `redact_auth_token_echo`'s rule since #315's re-review: an empty quoted
value (clap's missing value) and the flag's own spelling are shown;
otherwise `argShown`, the narrow rule. -/
def argShownFixed (before fromFlag : List (List Nat)) (q : List Nat) : Bool :=
  q.isEmpty || q == authFlagBytes || argShown before fromFlag q

/-- The clap errors that quote an argv element, as `redact_auth_token_echo`
handles them: an unexpected argument (`InvalidArg` of `UnknownArgument`), an
invalid value of the flag `flag` (`InvalidValue`), a value given to a flag
that takes no more (`TooManyValues`, quoted through `InvalidValue`), an
unrecognized subcommand (`InvalidSubcommand`). -/
inductive Quote where
  | unexpected
  | invalidValue (flag : String)
  | tooManyValues (flag : String)
  | subcommand
  deriving DecidableEq, Repr

/-- clap's first line for a quoted element `s`. A value parser's own source
message after an invalid value (`: invalid digit found in string`) is not
modelled: the parsers of the flags `serve` and `serve-web` accept (`u16`,
`u32`, `usize`, `IpAddr`, strings, paths) never echo their input there. -/
def quoteText : Quote → String → String
  | .unexpected, s => "unexpected argument '" ++ s ++ "' found"
  | .invalidValue f, s =>
    if s.isEmpty then "a value is required for '" ++ f ++ "' but none was supplied"
    else "invalid value '" ++ s ++ "' for '" ++ f ++ "'"
  | .tooManyValues f, s => "unexpected value '" ++ s ++ "' for '" ++ f ++ "' found; no more were expected"
  | .subcommand, s => "unrecognized subcommand '" ++ s ++ "'"

/-- The tip that replaces clap's suggestions on a redaction. -/
def redactTip : String :=
  "an argument after --auth-token is not shown (it may be part of the token); check the arguments after it, and quote a token that contains a space"

/-- A redacted error. Its usage line (not modelled) is the subcommand's plain
usage, the same for every argv, since clap's own can name its did-you-mean
flag for the redacted element. -/
def redactedText (k : Quote) : String :=
  quoteText k "<redacted>" ++ "\n\n  tip: " ++ redactTip

/-- What `main` prints for a parse error quoting `q`, `before` and `fromFlag`
being the argv elements before and from the first `--auth-token`: the
combined render of the rule and the text. -/
def quotedRender (k : Quote) (before fromFlag : List (List Nat)) (q : List Nat) : String :=
  if argShownFixed before fromFlag q then quoteText k (String.ofList (q.map Char.ofNat))
  else redactedText k

/-- The paths on which `lambo serve` and `lambo serve-web` print something
derived from the token alone (a reading of the code, not a proof of
completeness): `AuthTokenArg`'s `Debug` (`Commands` derives `Debug`), the
`Commands::Serve` field, the plan's `SecretToken` `Debug`, the
`http_auth_token_flag` refusal, the `LAMBO_AUTH_TOKEN` refusal of
`resolve_auth_token`, and (since #315's fix) `serve-web`'s `--auth-token`
refusal from `WebAuthTokenParser` and, on both commands, clap's
unexpected-argument error for an element after `--auth-token`
(`unexpectedArg`, its argument being that element, a piece of the token,
in the case the rule redacts). Neither `AuthTokenArg` nor `SecretToken`
implements `Display`. Two `serve-web` paths are not listed separately: its
`AuthToken` `Debug` (`Commands::ServeWeb`'s field) is constant like
`debugArg`, and its `LAMBO_AUTH_TOKEN` refusal (`serve_web::auth`'s
`resolve_auth_token_from`) prints `envRefusal`'s text behind a
`lambo serve-web: ` prefix. -/
inductive PrintPath where
  | debugArg
  | debugServeField
  | debugPlanToken
  | flagRefusal
  | envRefusal
  | webFlagRefusal
  | unexpectedArg
  deriving DecidableEq, Repr

def render : PrintPath → TokenRaw → String
  | .debugArg, _ => "AuthTokenArg(<redacted>)"
  | .debugServeField, _ => "auth_token: Some(AuthTokenArg(<redacted>))"
  | .debugPlanToken, r =>
    match secretNew r with
    | .ok _ => "Some(SecretToken(<redacted>))"
    | .error _ => "None"
  | .flagRefusal, r =>
    match verdict r with
    | some w => "invalid value for '--auth-token <TOKEN>': " ++ whyText w ++ " (the value is not shown)"
    | none => ""
  | .envRefusal, r =>
    match verdict r with
    | some .notUtf8 => "LAMBO_AUTH_TOKEN: is not valid UTF-8"
    | some w => "LAMBO_AUTH_TOKEN: " ++ whyText w
    | none => ""
  | .webFlagRefusal, r => serveWebFlagRefusalFixed r
  | .unexpectedArg, _ => redactedText .unexpected

/-- Redaction, as noninterference (#273's "AuthTokenArg redaction"
constraint): on each `PrintPath` the output depends on the token only through
its verdict class, never on its bytes. This holds by definition of `render`;
what it adds is the list of paths. (A substring statement would be false: a
token spelled `redacted` occurs in `AuthTokenArg(<redacted>)`.) Since #315's
fix the list includes `serve-web`'s flag refusal (`webFlagRefusal`) and the
unexpected-argument error for an element after `--auth-token`
(`unexpectedArg`, the redacted case; `quotedRender_unexpected_is_render`
connects it to the argv-level `quotedRender`, whose own noninterference is
`quotedRender_noninterference`). Not covered:
`credential_refusal_reveals_equality`; the previous versions
(`serve_web_flag_refusal_leaks`, `unexpected_arg_leaked`,
`hyphen_token_leaked`, `b1_fail_open`); and what a token split by an
unquoted space still reveals, which depends on the argv's shape, not on the
token's verdict: that it was split (the error fires at all); a tail piece
that is a real flag changes the error (`--port <piece>` names `--port`, and a
piece starting `-h` prints help with exit 0); a piece the command accepts is
accepted, leaving the weaker head as the token, and may be printed as that
flag's value (`--transport <piece>` prints "unknown transport" with it, a
`--session <piece>` appears in serve-web's banner); and a spaced
`--auth-token -<x>` errors differently depending on whether `-<x>` is a
known flag ("a value is required" or an unexpected argument). All of these
predate #315. -/
theorem render_noninterference (p : PrintPath) {r r' : TokenRaw}
    (h : verdict r = verdict r') : render p r = render p r' := by
  cases p with
  | debugArg => rfl
  | debugServeField => rfl
  | debugPlanToken =>
    unfold verdict at h
    simp only [render]
    split <;> split <;> simp_all
  | flagRefusal => simp only [render, h]
  | envRefusal => simp only [render, h]
  | webFlagRefusal =>
    simp only [render, serveWebFlagRefusalFixed, parseWebAuthArg, h]
    cases verdict r' <;> rfl
  | unexpectedArg => rfl

/-- The two `Debug` paths are constant: they print the same for every
token, valid or not. -/
theorem debug_constant (r r' : TokenRaw) :
    render .debugArg r = render .debugArg r' ∧
      render .debugServeField r = render .debugServeField r' := ⟨rfl, rfl⟩

/-- `check_credential_set`'s legacy-token refusal (`lambo serve` over HTTP),
for a credential set that passes its other checks: it names the first
credential whose token equals the legacy one, never a value. -/
def credRefusal (legacy : List Nat) (cs : List (Nat × List Nat)) : String :=
  match cs.find? (fun c => c.2 == legacy) with
  | some c => "credential " ++ toString c.1 ++
      " resolves to the same token as --auth-token / LAMBO_AUTH_TOKEN (the legacy \"default\" credential); give each its own, or drop the legacy token (the value is not shown here)"
  | none => ""

/-- Outside `render_noninterference`: the credential refusal's output
depends on whether the legacy token equals a configured credential's token
(one bit, and the credential's name), though both tokens below are valid and
the bytes are never printed. -/
theorem credential_refusal_reveals_equality :
    verdict ⟨[97], true⟩ = verdict ⟨[98], true⟩ ∧
      credRefusal [97] [(2, [97])] ≠ credRefusal [98] [(2, [97])] := by decide

/-- The previous version (before #315's fix) of `lambo serve-web
--auth-token`'s clap error: its parser was `AuthToken: FromStr`, so clap's
`ValueValidation` error quoted the value it refused. Bytes are read as code
points (ASCII here). The fixed error is `serveWebFlagRefusalFixed`. -/
def serveWebFlagRefusal (r : TokenRaw) : String :=
  match verdict r with
  | some w => "invalid value '" ++ String.ofList (r.bytes.map Char.ofNat) ++
      "' for '--auth-token <TOKEN>': " ++ whyText w
  | none => ""

set_option maxRecDepth 20000 in
/-- Shown about the Rust before #315's fix: `lambo serve-web --auth-token
'x '` printed the refused token in clap's error, so redaction did not hold
there; two tokens refused for the same reason printed differently. The fix is
`serve_web_flag_refusal_fixed`. -/
theorem serve_web_flag_refusal_leaks :
    serveWebFlagRefusal ⟨[120, 32], true⟩ =
        "invalid value 'x ' for '--auth-token <TOKEN>': " ++ whyText .surroundingSpace ∧
      verdict ⟨[120, 32], true⟩ = verdict ⟨[121, 32], true⟩ ∧
      serveWebFlagRefusal ⟨[120, 32], true⟩ ≠ serveWebFlagRefusal ⟨[121, 32], true⟩ := by
  decide

/-! ### The fix for #315

`lambo serve-web --auth-token` is parsed by `WebAuthTokenParser`
(`parseWebAuthArg`), whose refusal is built by `auth_token_refusal`, the
builder `lambo serve`'s `http_auth_token_flag` uses. Its clap error is
`serveWebFlagRefusalFixed`, and `render_noninterference` covers it as
`PrintPath.webFlagRefusal`. What the fix keeps: the tokens refused and the
token handed on (`parseWebAuthArg_ok_iff`, `parseWebAuthArg_error_iff`), and
the reason (`web_flag_refusal_keeps_reason`). -/

/-- The fixed parser accepts exactly the tokens `secretNew` accepts, which
are the tokens `AuthToken: FromStr` accepted, and hands the token on as
given. -/
theorem parseWebAuthArg_ok_iff (r v : TokenRaw) :
    parseWebAuthArg r = .ok v ↔ verdict r = none ∧ v = r := by
  unfold parseWebAuthArg
  cases verdict r with
  | none => simp [eq_comm]
  | some w => simp

/-- The fixed parser refuses for the rule the token broke, the one the
previous version's error named. -/
theorem parseWebAuthArg_error_iff (r : TokenRaw) (w : Why) :
    parseWebAuthArg r = .error w ↔ verdict r = some w := by
  unfold parseWebAuthArg
  cases verdict r <;> simp

/-- `serve-web`'s flag refusal is `serve`'s, word for word: one builder. -/
theorem web_flag_refusal_eq_serve (r : TokenRaw) :
    render .webFlagRefusal r = render .flagRefusal r := by
  simp only [render, serveWebFlagRefusalFixed, parseWebAuthArg]
  cases verdict r <;> rfl

/-- The fix keeps the reason: a refused token's fixed error names the rule
the previous version's error named, and both are empty exactly on a valid
token. -/
theorem web_flag_refusal_keeps_reason (r : TokenRaw) :
    (∀ w, verdict r = some w →
      serveWebFlagRefusalFixed r =
          "invalid value for '--auth-token <TOKEN>': " ++ whyText w ++ " (the value is not shown)" ∧
        ∃ pre, serveWebFlagRefusal r = pre ++ whyText w) ∧
      (verdict r = none → serveWebFlagRefusalFixed r = "" ∧ serveWebFlagRefusal r = "") := by
  refine ⟨fun w h => ?_, fun h => ?_⟩
  · refine ⟨by simp [serveWebFlagRefusalFixed, parseWebAuthArg, h], ?_⟩
    refine ⟨"invalid value '" ++ String.ofList (r.bytes.map Char.ofNat) ++
      "' for '--auth-token <TOKEN>': ", ?_⟩
    simp only [serveWebFlagRefusal, h]
  · simp [serveWebFlagRefusalFixed, parseWebAuthArg, serveWebFlagRefusal, h]

set_option maxRecDepth 20000 in
/-- **#315 fixed**, on the issue's inputs: `lambo serve-web --auth-token
'x '` and `--auth-token 'NOTASECRET-probe '` print the same line, which names
the rule and not the value; a control byte is refused by its own rule, a
valid token prints nothing, and the line is `lambo serve`'s. -/
theorem serve_web_flag_refusal_fixed :
    serveWebFlagRefusalFixed ⟨[120, 32], true⟩ =
        "invalid value for '--auth-token <TOKEN>': " ++ whyText .surroundingSpace ++
          " (the value is not shown)" ∧
      serveWebFlagRefusalFixed
          ⟨[78, 79, 84, 65, 83, 69, 67, 82, 69, 84, 45, 112, 114, 111, 98, 101, 32], true⟩ =
        serveWebFlagRefusalFixed ⟨[120, 32], true⟩ ∧
      serveWebFlagRefusalFixed ⟨[120, 32], true⟩ = serveWebFlagRefusalFixed ⟨[121, 32], true⟩ ∧
      serveWebFlagRefusalFixed ⟨[120, 1, 121], true⟩ =
        "invalid value for '--auth-token <TOKEN>': " ++ whyText .notPrintable ++
          " (the value is not shown)" ∧
      serveWebFlagRefusalFixed ⟨[120], true⟩ = "" ∧
      render .webFlagRefusal ⟨[120, 32], true⟩ = render .flagRefusal ⟨[120, 32], true⟩ := by
  decide

/-! ### The fix for #315's unexpected-argument path

Found in review of the fix above: clap's "unexpected argument '<it>' found"
quoted a token that starts with `-` (taken for a flag) or the tail of an
unquoted token split by a space, on `serve` and `serve-web` alike. a26ffd30
took a `-`-leading element as the value (`flagTakesHyphen`, fail open, so
the re-review reverted it: `flagTakes`, `authFlagValue`) and has `main`
print the parse error through `redact_auth_token_echo` (a26ffd30:
`unexpectedArgRefusalFixed`, `argShown`; now `quotedRender`,
`argShownFixed`). -/

set_option maxRecDepth 20000 in
/-- Shown about the Rust before the fix: `--auth-token --x` printed
"unexpected argument '--x' found" (and `-x...` its first short flag,
`-x`). -/
theorem hyphen_token_leaked :
    flagTakesPrev ⟨[45, 45, 120], true⟩ = false ∧
      unexpectedArgRefusal ⟨[45, 45, 120], true⟩ = "unexpected argument '--x' found" ∧
      flagTakesPrev ⟨[45, 120], true⟩ = false := by
  decide

set_option maxRecDepth 20000 in
/-- Shown about the Rust before the fix: the tail of a split token was
printed, so two tails printed differently. -/
theorem unexpected_arg_leaked :
    unexpectedArgRefusal ⟨[120], true⟩ = "unexpected argument 'x' found" ∧
      unexpectedArgRefusal ⟨[120], true⟩ ≠ unexpectedArgRefusal ⟨[121], true⟩ := by
  decide

/-- About a26ffd30 (previous version): every element after `--auth-token`
was its value, agreeing with the base wherever that took the element. -/
theorem flagTakesHyphen_total (r : TokenRaw) :
    flagTakesHyphen r = true ∧
      (flagTakesPrev r = true → flagTakesHyphen r = flagTakesPrev r) :=
  ⟨rfl, fun h => h.symm ▸ rfl⟩

/-- The redacted unexpected-argument error is the same for every element. -/
theorem unexpected_arg_constant (a a' : TokenRaw) :
    render .unexpectedArg a = render .unexpectedArg a' := rfl

/-- The narrow rule never shows an element that occurs from the flag on. -/
theorem argShown_from_flag {before fromFlag : List (List Nat)} {q : List Nat}
    (h : fromFlag.any (infixOf q) = true) : argShown before fromFlag q = false := by
  simp [argShown, h]

set_option maxRecDepth 20000 in
/-- **The review's probes, fixed**: `--auth-token NOTASECRET-a NOTASECRET-extra`
prints the same line as any other tail; a mistyped `--sesion` before the
flag is still shown; a tail that also occurs before the flag is not. -/
theorem unexpected_arg_fixed :
    unexpectedArgRefusalFixed
        ⟨[78, 79, 84, 65, 83, 69, 67, 82, 69, 84, 45, 101, 120, 116, 114, 97], true⟩ =
      unexpectedArgRefusalFixed ⟨[120], true⟩ ∧
      argShown [[45, 45, 115, 101, 115, 105, 111, 110]] [[116, 111, 107]]
        [45, 45, 115, 101, 115, 105, 111, 110] = true ∧
      argShown [[98]] [[97], [98]] [98] = false := by
  decide

/-! ### #315 re-review: fail closed, and the combined render

`allow_hyphen_values` made a flag after `--auth-token` the token, so
`--auth-token $UNSET --allow-embedding-mismatch` started a server whose
token was the flag. It is removed: a spaced `--auth-token` refuses an
element that looks like a flag (`authFlagValue_spaced_none_iff`), as the
base did (`flagTakes_agrees_base`), and `--auth-token=-abc` takes any value
(`authFlagValue_eq_form`). The redaction covers what that refusal quotes:
`quotedRender_noninterference` over every quoting kind. -/

set_option maxRecDepth 20000 in
/-- Shown about a26ffd30 (previous version): `--auth-token -h` made `-h` the
token, and `--auth-token --port=1` made `--port=1` the token. -/
theorem b1_fail_open :
    authFlagValueHyphen false ⟨[45, 104], true⟩ = some ⟨[45, 104], true⟩ ∧
      authFlagValueHyphen false ⟨[45, 45, 112, 111, 114, 116, 61, 49], true⟩ =
        some ⟨[45, 45, 112, 111, 114, 116, 61, 49], true⟩ := by
  decide

/-- A spaced `--auth-token` is refused exactly when the next element looks
like a flag. -/
theorem authFlagValue_spaced_none_iff (r : TokenRaw) :
    authFlagValue false r = none ↔ looksLikeFlag r = true := by
  unfold authFlagValue flagTakes
  cases looksLikeFlag r <;> simp

/-- `--auth-token=<v>` takes `v`, a leading `-` included. -/
theorem authFlagValue_eq_form (r : TokenRaw) : authFlagValue true r = some r := by
  simp [authFlagValue]

/-- The fix restores the base's rule (`flagTakesPrev`), except that a lone
`-`, which clap takes as a value, the base model did not. -/
theorem flagTakes_agrees_base (r : TokenRaw) (h : r.bytes ≠ [45]) :
    flagTakes r = flagTakesPrev r := by
  rcases r with ⟨bs, u⟩
  cases bs with
  | nil => rfl
  | cons b t =>
    by_cases hb : b = 45
    · subst hb
      cases t with
      | nil => exact absurd rfl h
      | cons c t => simp [flagTakes, looksLikeFlag, flagTakesPrev]
    · have ho : (some b == some 45) = false := by simp [hb]
      simp [flagTakes, looksLikeFlag, flagTakesPrev, bne, ho]

/-- An element from the flag on, neither empty nor the flag's spelling, is
never shown. -/
theorem argShownFixed_from_flag {before fromFlag : List (List Nat)} {q : List Nat}
    (h0 : q ≠ []) (h1 : q ≠ authFlagBytes) (h : fromFlag.any (infixOf q) = true) :
    argShownFixed before fromFlag q = false := by
  have h0' : q.isEmpty = false := by cases q <;> simp_all
  simp [argShownFixed, h0', h1, argShown_from_flag h]

theorem quotedRender_hidden {k : Quote} {before fromFlag : List (List Nat)} {q : List Nat}
    (h : argShownFixed before fromFlag q = false) :
    quotedRender k before fromFlag q = redactedText k := by
  simp [quotedRender, h]

/-- Under the fixed echo rule, `redact_auth_token_echo` renders a shown
argument quoted exactly as typed. -/
theorem quotedRender_shown {k : Quote} {before fromFlag : List (List Nat)} {q : List Nat}
    (h : argShownFixed before fromFlag q = true) :
    quotedRender k before fromFlag q = quoteText k (String.ofList (q.map Char.ofNat)) := by
  simp [quotedRender, h]

/-- **Noninterference over argv**: for each quoting kind, every element from
the flag on (other than an empty value and the flag's spelling) prints the
same error. -/
theorem quotedRender_noninterference (k : Quote) {before fromFlag : List (List Nat)}
    {q q' : List Nat}
    (h0 : q ≠ []) (h1 : q ≠ authFlagBytes) (h : fromFlag.any (infixOf q) = true)
    (h0' : q' ≠ []) (h1' : q' ≠ authFlagBytes) (h' : fromFlag.any (infixOf q') = true) :
    quotedRender k before fromFlag q = quotedRender k before fromFlag q' := by
  rw [quotedRender_hidden (argShownFixed_from_flag h0 h1 h),
    quotedRender_hidden (argShownFixed_from_flag h0' h1' h')]

/-- The argv-level render is `PrintPath.unexpectedArg`'s on the elements it
redacts. -/
theorem quotedRender_unexpected_is_render {before fromFlag : List (List Nat)}
    {q : List Nat} (h0 : q ≠ []) (h1 : q ≠ authFlagBytes)
    (h : fromFlag.any (infixOf q) = true) :
    quotedRender .unexpected before fromFlag q = render .unexpectedArg ⟨q, true⟩ := by
  rw [quotedRender_hidden (argShownFixed_from_flag h0 h1 h)]
  rfl

/-- A missing value keeps clap's message, with no tip. -/
theorem missing_value_kept (f : String) (before fromFlag : List (List Nat)) :
    quotedRender (.invalidValue f) before fromFlag [] =
      "a value is required for '" ++ f ++ "' but none was supplied" := by
  simp [quotedRender, argShownFixed, quoteText]

set_option maxRecDepth 20000 in
/-- **The re-review's probes, fixed**: a spaced `--auth-token` refuses `-h`,
`--port=1` and `-x` and takes a lone `-`; `--auth-token=-x` takes `-x`; an
invalid `--port` value, an unrecognized subcommand and a value given to
`--allow-embedding-mismatch` after the flag are redacted; `lambo --auth-token x serve-web` names `--auth-token`; a missing
value says so. -/
theorem re_review_fixed :
    authFlagValue false ⟨[45, 104], true⟩ = none ∧
      authFlagValue false ⟨[45, 45, 112, 111, 114, 116, 61, 49], true⟩ = none ∧
      authFlagValue false ⟨[45, 120], true⟩ = none ∧
      authFlagValue false ⟨[45], true⟩ = some ⟨[45], true⟩ ∧
      authFlagValue true ⟨[45, 120], true⟩ = some ⟨[45, 120], true⟩ ∧
      quotedRender (.invalidValue "--port <PORT>") [] [[97], [45, 45, 112, 111, 114, 116], [78, 79]]
          [78, 79] = redactedText (.invalidValue "--port <PORT>") ∧
      quotedRender .subcommand [] [[78, 79]] [78, 79] = redactedText .subcommand ∧
      quotedRender (.tooManyValues "--allow-embedding-mismatch") [] [[97], [78, 79]] [78, 79] =
        redactedText (.tooManyValues "--allow-embedding-mismatch") ∧
      quotedRender .unexpected [] [authFlagBytes, [120]] authFlagBytes =
        "unexpected argument '--auth-token' found" ∧
      quotedRender (.invalidValue "--auth-token <TOKEN>") [] [authFlagBytes] [] =
        "a value is required for '--auth-token <TOKEN>' but none was supplied" := by
  decide

end Lambo.Resolve
