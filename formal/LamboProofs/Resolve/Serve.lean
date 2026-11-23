import LamboProofs.Resolve.Construction

/-
# Serve preflight and `main`'s routing (`src/main.rs`)

Models `ServePlan`, `serve_preflight`, `stdio_session` and
`too_many_stdio_sessions` (`Plan`, `preflight` and its steps) with
`pin_sessions` (`src/mcp/serve/pinned.rs`), `plan_layers`,
`resolve_auth_token`, `SecretToken::new` / `check_configured_token`
(`src/surface/bearer.rs`, `secretNew`, `maxBearer`) and `check_credential_set`
(`src/config/credential.rs`, `credsOk`), the ledger flags (`check_ledger`,
`ledgerStep`) and the bind (`check_bind`, `bindStep`), which `serve_preflight`
refuses before any backend is built (#316); what `lambo::mcp::serve` refuses
again for library callers, after the backends are built and before any lease
(`authorize_bind`, `authorize_ledger`: `serveStartRefused`); and `fn main` from
the parsed command on (`main`): the `canon` route, the serve and serve-web
pre-reads (serve-web's bind check, `plan_bind`, is `webBindStep`), the
resolve over `Construction.lean`, the
`unreachable!("writer override requires full backends")`, the final
`match (cmd, resolved)` and its `_` arm ("internal resolve mismatch"). Session
names are `Nat`s and `addr` stands for `parse_addressed`; stdio session
selection from `[[serve.projects]]` is an input (`StdioSel`); credential names
`0` and `1` stand for the reserved `default` and `local`. These are the
serve-bootstrap module and the thin router #273 proposes.

Results: the final `_` arm and the mismatch-override `unreachable!` are
unreachable (`resolve_ok_arm`, `main_never_mismatch_or_panic`); `main` builds
nothing the resolve did not (`main_built_allowed`, by construction); `canon`
reads no `lambo.toml` and builds no backend from it only because `main` routes
it first (`main_canon`, `resolve_canon_would_build_store`); an unknown key exits
1 for every command that reads the file, before anything is built
(`unknown_key_exit1`); a refused `session classify` or `split` is labelled
"failed to resolve store" (`session_errors_labelled_store`). Preflight
refusals exit 2, except an unreadable file and an undecidable
`[[serve.projects]]` map (1) (`preflight_exit_codes`, `preflight_exit1`); stdio
with two sessions exits 2 before the file is read and ignores tokens
(`preflight_stdio_many`, `preflight_stdio_ignores_tokens`); an invalid
`--auth-token` refuses HTTP even when `LAMBO_AUTH_TOKEN` would win, a set but
invalid variable is never a fallback to the flag, and a valid variable wins
(`preflight_bad_flag`, `preflight_bad_env`, `preflight_env_wins`); every plan
satisfies `preflight_ok_stdio` / `preflight_ok_http`, and a preflight refusal
costs no backend build (`serve_preflight_refusal_builds_nothing`). Since #316
a plan preflight passes is never refused by `serve`'s own start checks
(`preflight_ok_start_allowed`), so a non-loopback bind with no credential and
`--ledger-heartbeat` without `--ledger` (or zero) exit 2 before any backend is
built, and `serve-web` refuses the same bind with the same code, also before
the build (`exposed_bind_refused_before_build`,
`heartbeat_without_ledger_refused_before_build`).

Boundary: exit codes are modelled only where `main` and `serve_preflight`
choose them; a command body's own exit codes (`CliError::exit_code`,
`canon_exit`), `run_canon`, `serve-web`'s session, host and credential
planning (an input, `MainIn.webPlan`; its bind check is modelled, with
"has a credential" an input, `MainIn.webCredential`), tracing setup, `[serve]`'s unenforced-key notice and
`session verify`'s `ServeConfig::validate` are not. Path matching, `~` and
symlinks in stdio session selection are not modelled. Tokens are byte lists
with a UTF-8 flag and ASCII whitespace; Rust's `str::trim` also trims non-ASCII
whitespace, which can change the reason given for a refused token but never
the verdict. What the token printing paths reveal is `Auth.lean`.
-/

namespace Lambo.Resolve
/-! ## Serve preflight -/

inductive Transport where
  | stdio
  | http
  deriving DecidableEq, Repr

/-- Why `SecretToken::new` (`check_configured_token`) or the UTF-8 check
before it refuses a token. -/
inductive Why where
  | notUtf8
  | empty
  | surroundingSpace
  | notPrintable
  | tooLong
  deriving DecidableEq, Repr

/-- `MAX_BEARER_CREDENTIAL_BYTES`.
Pinned by `formal_constants_tests::resolve`. -/
def maxBearer : Nat := 4096

/-- ASCII whitespace, which is what `str::trim` removes from an ASCII
string. -/
def isAsciiWs (b : Nat) : Bool := b == 32 || (9 ≤ b && b ≤ 13)

def printable (b : Nat) : Bool := 0x20 ≤ b && b ≤ 0x7e

/-- `SecretToken::new` on a raw token, after the caller's UTF-8 check. -/
def secretNew (r : TokenRaw) : Except Why (List Nat) :=
  if !r.utf8 then .error .notUtf8
  else if r.bytes.all isAsciiWs then .error .empty
  else if r.bytes.head?.any isAsciiWs || r.bytes.getLast?.any isAsciiWs then
    .error .surroundingSpace
  else if !r.bytes.all printable then .error .notPrintable
  else if maxBearer < r.bytes.length then .error .tooLong
  else .ok r.bytes

/-- `check_credential_set`: no reserved name (`0` = `default`, `1` =
`local`), no two credentials with one name or one token, none with the
legacy token. -/
def credsOk (legacy : Option (List Nat)) (cs : List (Nat × List Nat)) : Bool :=
  cs.all (fun c => c.1 != 0 && c.1 != 1) && decide (cs.map (·.1)).Nodup &&
    decide (cs.map (·.2)).Nodup &&
    (match legacy with
     | some t => !(cs.map (·.2)).contains t
     | none => true)

/-- The ordered union of `--session` and `[serve] sessions`. -/
def dedup (l : List Nat) : List Nat :=
  l.foldl (fun acc n => if acc.contains n then acc else acc ++ [n]) []

/-- `pin_sessions` = `plan_sessions` then `check_pinned`. `addr` is
`parse_addressed` (1 to 128 bytes of `[A-Za-z0-9._:-]`, no leading `.`). -/
def pinSessions (addr : Nat → Bool) (cli : List Nat) (cfg : ServeCfg) :
    Transport → Except Unit (Nat × List Nat)
  | .stdio =>
    match cli with
    | [one] => .ok (one, [one])
    | _ => .error ()
  | .http =>
    let ss := dedup (cli ++ cfg.sessions)
    match ss with
    | [] => .error ()
    | first :: _ =>
      let d := (cli.head?.or cfg.defaultSession).getD first
      if !ss.contains d then .error ()
      else if cfg.maxAttached < ss.length then .error ()
      else if 1 < ss.length && !ss.all addr then .error ()
      else .ok (d, ss)

/-- `lambo serve`'s own flags. `transport = none` is a string that is
neither `stdio` nor `http` (trimmed, any case); `authEnv` is
`LAMBO_AUTH_TOKEN` (`none`: not set; an exported empty one is
`some ⟨[], true⟩`). -/
structure ServeIn where
  transport : Option Transport := some .stdio
  sessions : List Nat := []
  authFlag : Option TokenRaw := none
  authEnv : Option TokenRaw := none
  loopback : Bool := true
  ledger : Bool := false
  heartbeat : Option Nat := none
  deriving DecidableEq, Repr

/-- `ServePlan`. -/
structure Plan where
  transport : Transport
  default : Nat
  sessions : List Nat
  token : Option (List Nat)
  creds : List (Nat × List Nat)
  file : Config
  deriving DecidableEq, Repr

/-- `http_auth_token_flag`: the flag is checked over HTTP only. -/
def flagStep (t : Transport) (fl : Option TokenRaw) : Except Nat (Option (List Nat)) :=
  match t, fl with
  | .http, some r =>
    match secretNew r with
    | .ok k => .ok (some k)
    | .error _ => .error 2
  | _, _ => .ok none

/-- `too_many_stdio_sessions`, before the file is read. -/
def countStep (t : Transport) (sessions : List Nat) : Except Nat Unit :=
  if t = .stdio ∧ 1 < sessions.length then .error 2 else .ok ()

def loadStep (raw : Raw) : Except Nat Config :=
  match load raw with
  | .error _ => .error 1
  | .ok c => .ok c

/-- `stdio_session`: the flag's one session, else the file's selection; a
missing one is clap's missing-`--session` usage error (exit 2), a map that
cannot decide a configuration error (exit 1). HTTP takes `--session` as
given. -/
def selStep (t : Transport) (sessions : List Nat) (c : Config) : Except Nat (List Nat) :=
  match t, sessions with
  | .http, ss => .ok ss
  | .stdio, [one] => .ok [one]
  | .stdio, [] =>
    match c.serve.stdioSel with
    | .project x => .ok [x]
    | .fallback x => .ok [x]
    | .missing => .error 2
    | .configError => .error 1
  | .stdio, _ => .error 2

def pinStep (addr : Nat → Bool) (ss : List Nat) (c : Config) (t : Transport) :
    Except Nat (Nat × List Nat) :=
  match pinSessions addr ss c.serve t with
  | .error _ => .error 2
  | .ok x => .ok x

/-- `plan_layers`: checked over HTTP only. -/
def layerStep (t : Transport) (c : Config) : Except Nat Unit :=
  if t = .http ∧ c.serve.layersOk = false then .error 2 else .ok ()

/-- `resolve_auth_token`: over HTTP a set `LAMBO_AUTH_TOKEN` wins and must
be valid; unset, the flag's token stands. Stdio has none. -/
def tokStep (t : Transport) (ev : Option TokenRaw) (flagTok : Option (List Nat)) :
    Except Nat (Option (List Nat)) :=
  match t, ev with
  | .http, some r =>
    match secretNew r with
    | .ok k => .ok (some k)
    | .error _ => .error 2
  | .http, none => .ok flagTok
  | .stdio, _ => .ok none

/-- `resolve_credentials` then `check_serve_credentials`, over HTTP only. -/
def credStep (t : Transport) (c : Config) (token : Option (List Nat)) :
    Except Nat (List (Nat × List Nat)) :=
  match t with
  | .stdio => .ok []
  | .http =>
    match c.serve.creds with
    | none => .error 2
    | some cs => if credsOk token cs then .ok cs else .error 2

/-- `check_ledger`: `--ledger-heartbeat` without `--ledger`, or a zero
interval. -/
def ledgerRefused (s : ServeIn) : Bool :=
  (s.heartbeat.isSome && !s.ledger) || s.heartbeat == some 0

/-- The ledger flags, checked before the file is read (exit 2). -/
def ledgerStep (s : ServeIn) : Except Nat Unit :=
  if ledgerRefused s then .error 2 else .ok ()

/-- `check_bind` (`authorize_bind` over the plan's legacy token and
credentials): an HTTP bind beyond loopback with no credential. -/
def bindRefused (t : Transport) (loopback : Bool) (token : Option (List Nat))
    (creds : List (Nat × List Nat)) : Bool :=
  t == .http && !loopback && token.isNone && creds.isEmpty

/-- The bind, checked once the token and credentials are resolved (exit
2). -/
def bindStep (t : Transport) (loopback : Bool) (token : Option (List Nat))
    (creds : List (Nat × List Nat)) : Except Nat Unit :=
  if bindRefused t loopback token creds then .error 2 else .ok ()

def transportStep (t : Option Transport) : Except Nat Transport :=
  match t with
  | none => .error 2
  | some t => .ok t

/-- `serve_preflight`, with `stdio_session` and `too_many_stdio_sessions`.
The error is the exit code; the message is printed by then. -/
def preflight (addr : Nat → Bool) (raw : Raw) (s : ServeIn) : Except Nat Plan := do
  let t ← transportStep s.transport
  let flagTok ← flagStep t s.authFlag
  countStep t s.sessions
  ledgerStep s
  let c ← loadStep raw
  let ss ← selStep t s.sessions c
  let dp ← pinStep addr ss c t
  layerStep t c
  let token ← tokStep t s.authEnv flagTok
  let cs ← credStep t c token
  bindStep t s.loopback token cs
  pure ⟨t, dp.1, dp.2, token, cs, c⟩

/-! ## `main` -/

/-- What `main` does with one invocation: exit with a code before or
instead of a command body, hand the command its backends (`runs`), route
`canon` (no resolve), hit the `unreachable!` of the mismatch override
(`panics`), or fall into the final `_` arm, "internal resolve mismatch"
(`mismatch`). -/
inductive Outcome where
  | exit (code : Nat)
  | runs (c : Cmd)
  | canon
  | panics
  | mismatch
  deriving DecidableEq, Repr

structure MainIn where
  cmd : Cmd
  inv : Inv := {}
  /-- `--allow-embedding-mismatch` (clap admits it only where
  `hasMismatchFlag`). -/
  mismatchFlag : Bool := false
  serve : ServeIn := {}
  /-- `serve-web`'s session, host and credential planning: `some n` is a
  refusal with exit code `n`. -/
  webPlan : Option Nat := none
  /-- `serve-web --bind` is a loopback address. -/
  webLoopback : Bool := true
  /-- `serve-web` has a credential: `LAMBO_AUTH_TOKEN` or `--auth-token`, or
  a configured or inherited credential. -/
  webCredential : Bool := false
  deriving DecidableEq, Repr

/-- The arms of `main`'s final `match (cmd, resolved)`. -/
def armMatches : Cmd → Resolved → Bool
  | .session .classify, .classification _ => true
  | .session .split, .split .. => true
  | .session .verify, .verify .. => true
  | .embed, .embedderOnly .. => true
  | .serve, .full .. | .serveWeb, .full .. | .demo, .full .. | .recall, .full ..
  | .derive, .full .. | .deriveImage, .full .. | .recordAction, .full ..
  | .reserve, .full .. | .release, .full .. | .reEmbed, .full .. => true
  | .saints, .storeOnly .. | .inspect, .storeOnly .. | .stats _, .storeOnly ..
  | .provision, .storeOnly .. | .recallIndex, .storeOnly .. | .names, .storeOnly ..
  | .eraseSession, .storeOnly .. => true
  | _, _ => false

/-- `lambo::mcp::serve`'s refusals before any lease, which run after the
backends are built: `authorize_bind` (an HTTP bind beyond loopback with no
credential) and `authorize_ledger` (`--ledger-heartbeat` without `--ledger`,
or zero). Exit 1. They stay in `serve` for library callers; `main` reaches
them only with a plan `serve_preflight` passed, which they never refuse
(`preflight_ok_start_allowed`). -/
def serveStartRefused (p : Plan) (s : ServeIn) : Bool :=
  bindRefused p.transport s.loopback p.token p.creds || ledgerRefused s

/-- `serve-web`'s bind check in `main`'s planning (`plan_bind`, the rule of
`authorize_bind_web`, which `run` applies again for library callers): a
non-loopback bind with no credential is a usage error, exit 2. -/
def webBindStep (loopback credential : Bool) : Except Nat Unit :=
  if !loopback && !credential then .error 2 else .ok ()

/-- `fn main`, from the parsed command on. Returns what was built and the
outcome. -/
def main (f : Features) (addr : Nat → Bool) (file : Nat → Raw) (m : MainIn) :
    List (Site × Built) × Outcome :=
  match m.cmd with
  | .canon => ([], .canon)
  | cmd =>
    let pre : Except Nat (Option Config × Option Plan) :=
      match cmd with
      | .serve =>
        match preflight addr (file 0) m.serve with
        | .error n => .error n
        | .ok p => .ok (some p.file, some p)
      | .serveWeb =>
        match load (file 0) with
        | .error _ => .error 1
        | .ok c =>
          match m.webPlan with
          | some n => .error n
          | none =>
            match webBindStep m.webLoopback m.webCredential with
            | .error n => .error n
            | .ok () => .ok (some c, none)
      | _ => .ok (none, none)
    match pre with
    | .error n => ([], .exit n)
    | .ok (loaded, plan) =>
      let r := resolveForCommand f file m.inv cmd loaded
      match r.result with
      | .error _ => (r.built, .exit 1)
      | .ok res =>
        if m.mismatchFlag && cmd.hasMismatchFlag && !res.isFull then (r.built, .panics)
        else if !armMatches cmd res then (r.built, .mismatch)
        else
          match plan with
          | some p => if serveStartRefused p m.serve then (r.built, .exit 1) else (r.built, .runs cmd)
          | none => (r.built, .runs cmd)

/-! ## Results: `main`'s dispatch is total -/

/-- Every successful resolve hands its command the `Resolved` variant that
command's arm matches (for every command `main` resolves, i.e. all but
`canon`). -/
theorem resolve_ok_arm (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) (hc : cmd ≠ .canon) {r : Resolved}
    (h : (resolveForCommand f file inv cmd loaded).result = .ok r) : armMatches cmd r = true := by
  by_cases hs : cmd.isSession = true
  · cases cmd <;> simp [Cmd.isSession] at hs
    rename_i a
    cases a <;>
      simp only [resolveForCommand, resolveClassify, resolveSplit, resolveVerify] at h <;>
      (repeat' (first | (split at h) | (cases h))) <;> rfl
  by_cases he : cmd = .embed
  · subst he
    obtain ⟨c, -, rfl, -⟩ := resolveEmbed_ok h
    rfl
  by_cases hn : cmd.needsEmbedder = true
  · obtain ⟨c, w, -, rfl, -⟩ := full_ok (by simp_all [Cmd.isFull]) h
    cases cmd <;> simp_all [Cmd.needsEmbedder, armMatches]
  · obtain ⟨c0, c1, -, -, rfl, -⟩ := storeOnly_ok (by simp_all [Cmd.isStoreOnly]) h
    cases cmd <;> simp_all [Cmd.needsEmbedder, armMatches, Cmd.isSession]

/-- The `_` arm of `main` ("internal resolve mismatch") is unreachable, and
so is the `unreachable!("writer override requires full backends")`: the
mismatch override exists only on commands that resolve full backends. -/
theorem main_never_mismatch_or_panic (f : Features) (addr : Nat → Bool) (file : Nat → Raw)
    (m : MainIn) : (main f addr file m).2 ≠ .mismatch ∧ (main f addr file m).2 ≠ .panics := by
  unfold main
  split
  · simp
  · rename_i cmd hcanon
    dsimp only
    split
    · simp
    · rename_i loaded plan _
      split
      · simp
      · rename_i res hres
        have harm := resolve_ok_arm f file m.inv m.cmd loaded hcanon hres
        have hfull : (m.mismatchFlag && m.cmd.hasMismatchFlag && !res.isFull) = false := by
          cases hm : m.cmd.hasMismatchFlag
          · simp
          · obtain ⟨hn, hne, -, -, hs⟩ := mismatchFlag_writer m.cmd hm
            have hsess : m.cmd.isSession = false := by
              cases hc : m.cmd <;> simp_all [Cmd.isSession]
            obtain ⟨c, w, -, rfl, -⟩ :=
              full_ok (by simp [Cmd.isFull, hn, hne, hsess]) hres
            simp [Resolved.isFull]
        simp only [hfull, harm, Bool.false_eq_true, ↓reduceIte, Bool.not_true]
        split
        · split <;> simp
        · simp

/-- Whatever the model's `main` builds is what the resolve built. This holds
by construction: the model gives command bodies no construction. For the
Rust it is a modelling assumption scoped to serving backends (no command body
builds a store or embedder from `lambo.toml`); `session split` and `canon`
bodies do build SQLite stores over explicit files, outside the model. Test
obligation: no non-test code under `src/cli` calls `build_store*`,
`build_embedder`, `resolve_backends`, `resolve_from_config_path` or
`resolve_store_only*`. -/
theorem main_built_allowed (f : Features) (addr : Nat → Bool) (file : Nat → Raw) (m : MainIn)
    {x : Site × Built} (hx : x ∈ (main f addr file m).1) : allowed m.cmd x = true := by
  unfold main at hx
  split at hx
  · simp at hx
  · rename_i cmd _
    dsimp only at hx
    split at hx
    · simp at hx
    · rename_i loaded plan _
      have := resolve_built_allowed f file m.inv m.cmd loaded (x := x)
      split at hx <;> (try split at hx) <;> (try split at hx) <;> (try split at hx) <;>
        (try split at hx) <;> exact this hx

/-- `lambo canon` reads no `lambo.toml` and builds no backend from it (#36);
its body's SQLite stores over the copied file are outside the model. -/
theorem main_canon (f : Features) (addr : Nat → Bool) (file : Nat → Raw) (m : MainIn)
    (h : m.cmd = .canon) : main f addr file m = ([], .canon) := by
  simp [main, h]

/-- Why `main` must route `canon` before the resolve (a constraint on the
#273 router): `resolve_for_command` itself would treat it as a store-only
command and build the configured store. -/
theorem resolve_canon_would_build_store :
    (resolveForCommand Features.dflt (fun _ => {}) {} .canon none).built =
      [(.storeOnly, .store .memory false)] := by decide

/-- An unknown `lambo.toml` key refuses every command that reads the file,
with exit 1, before anything is built. -/
theorem unknown_key_exit1 (f : Features) (addr : Nat → Bool) (file : Nat → Raw) (m : MainIn)
    (hk : (file 0).unknownKey = true) (hc : m.cmd ≠ .canon) (hs : m.cmd.isSession = false)
    (hserve : m.cmd ≠ .serve) : main f addr file m = ([], .exit 1) := by
  have hl := load_unknownKey (file 0) hk
  cases h : m.cmd <;> simp_all [main, resolveForCommand, resolveFull, resolveStoreRun,
    resolveEmbed, Cmd.needsEmbedder, Cmd.isSession]

/-- Error labels: `main`'s `what` for a refused resolve. -/
def errLabel (cmd : Cmd) : String :=
  if cmd == .session .verify then "failed to resolve verify inputs"
  else if cmd == .embed then "failed to resolve embedder"
  else if cmd.needsEmbedder then "failed to build backends"
  else "failed to resolve store"

/-- Shown about the Rust: a refused `session classify` or `session split`
(for example `--config is not supported`) is labelled "failed to resolve
store", though neither resolves a store. -/
theorem session_errors_labelled_store :
    errLabel (.session .classify) = "failed to resolve store" ∧
      errLabel (.session .split) = "failed to resolve store" := by decide

/-! ## Results: serve preflight -/

/-- Every refusal exits 1 or 2. -/
def CodeOk {α : Type} (x : Except Nat α) : Prop := ∀ n, x = .error n → n = 1 ∨ n = 2

theorem codeOk_bind {α β : Type} {x : Except Nat α} {g : α → Except Nat β} (hx : CodeOk x)
    (hg : ∀ a, CodeOk (g a)) : CodeOk (x >>= g) := by
  intro n h
  cases x with
  | error e => simp [bind, Except.bind] at h; subst h; exact hx _ rfl
  | ok a => exact hg a n h

theorem bind_ok {α β : Type} {x : Except Nat α} {g : α → Except Nat β} {b : β}
    (h : (x >>= g) = .ok b) : ∃ a, x = .ok a ∧ g a = .ok b := by
  cases x with
  | error e => simp [bind, Except.bind] at h
  | ok a => exact ⟨a, rfl, h⟩

macro "code_tac" : tactic =>
  `(tactic| (intro n h; repeat' (first | (split at h) | (cases h <;> simp))))

theorem transportStep_code (t : Option Transport) : CodeOk (transportStep t) := by
  unfold transportStep; code_tac
theorem flagStep_code (t : Transport) (fl : Option TokenRaw) : CodeOk (flagStep t fl) := by
  unfold flagStep; code_tac
theorem countStep_code (t : Transport) (ss : List Nat) : CodeOk (countStep t ss) := by
  unfold countStep; code_tac
theorem loadStep_code (raw : Raw) : CodeOk (loadStep raw) := by
  unfold loadStep; code_tac
theorem selStep_code (t : Transport) (ss : List Nat) (c : Config) : CodeOk (selStep t ss c) := by
  unfold selStep; code_tac
theorem pinStep_code (addr : Nat → Bool) (ss : List Nat) (c : Config) (t : Transport) :
    CodeOk (pinStep addr ss c t) := by
  unfold pinStep; code_tac
theorem layerStep_code (t : Transport) (c : Config) : CodeOk (layerStep t c) := by
  unfold layerStep; code_tac
theorem tokStep_code (t : Transport) (ev : Option TokenRaw) (ft : Option (List Nat)) :
    CodeOk (tokStep t ev ft) := by
  unfold tokStep; code_tac
theorem credStep_code (t : Transport) (c : Config) (tok : Option (List Nat)) :
    CodeOk (credStep t c tok) := by
  unfold credStep; code_tac
theorem ledgerStep_code (s : ServeIn) : CodeOk (ledgerStep s) := by
  unfold ledgerStep; code_tac
theorem bindStep_code (t : Transport) (lb : Bool) (tok : Option (List Nat))
    (cs : List (Nat × List Nat)) : CodeOk (bindStep t lb tok cs) := by
  unfold bindStep; code_tac

/-- `serve_preflight`'s refusals are usage errors (exit 2) except an
unreadable file and an undecidable `[[serve.projects]]` map (exit 1). -/
theorem preflight_exit_codes (addr : Nat → Bool) (raw : Raw) (s : ServeIn) :
    CodeOk (preflight addr raw s) := by
  unfold preflight
  refine codeOk_bind (transportStep_code _) fun t => ?_
  refine codeOk_bind (flagStep_code _ _) fun ft => ?_
  refine codeOk_bind (countStep_code _ _) fun _ => ?_
  refine codeOk_bind (ledgerStep_code _) fun _ => ?_
  refine codeOk_bind (loadStep_code _) fun c => ?_
  refine codeOk_bind (selStep_code _ _ _) fun ss => ?_
  refine codeOk_bind (pinStep_code _ _ _ _) fun dp => ?_
  refine codeOk_bind (layerStep_code _ _) fun _ => ?_
  refine codeOk_bind (tokStep_code _ _ _) fun tok => ?_
  refine codeOk_bind (credStep_code _ _ _) fun cs => ?_
  refine codeOk_bind (bindStep_code _ _ _ _) fun _ => ?_
  intro n h; cases h

theorem bind_error {α β : Type} {x : Except Nat α} {g : α → Except Nat β} {n : Nat}
    (h : (x >>= g) = .error n) : x = .error n ∨ ∃ a, x = .ok a ∧ g a = .error n := by
  cases x with
  | error e => simp [bind, Except.bind] at h; subst h; exact Or.inl rfl
  | ok a => exact Or.inr ⟨a, rfl, h⟩

theorem map_error {α β : Type} {x : Except Nat α} {g : α → β} {n : Nat}
    (h : (g <$> x) = .error n) : x = .error n := by
  cases x with
  | error e => simp [Functor.map, Except.map] at h; subst h; rfl
  | ok a => simp [Functor.map, Except.map] at h

theorem map_ok {α β : Type} {x : Except Nat α} {g : α → β} {b : β}
    (h : (g <$> x) = .ok b) : ∃ a, x = .ok a ∧ g a = b := by
  cases x with
  | error e => simp [Functor.map, Except.map] at h
  | ok a => simp [Functor.map, Except.map] at h; exact ⟨a, rfl, h⟩

theorem transportStep_ok {x : Option Transport} {t : Transport} (h : transportStep x = .ok t) :
    x = some t := by
  unfold transportStep at h; split at h <;> simp_all

theorem loadStep_ok {raw : Raw} {c : Config} (h : loadStep raw = .ok c) : load raw = .ok c := by
  unfold loadStep at h; split at h <;> simp_all

macro "no1_tac" h:ident : tactic =>
  `(tactic| repeat' (first | (split at $h:ident) | (simp at $h:ident; done)))

/-- The exit-1 refusals are exactly the file's: it does not load, or a
stdio serve's `[[serve.projects]]` map cannot decide (the test
`a_map_that_cannot_be_applied_is_a_config_error_exiting_1`). -/
theorem preflight_exit1 {addr : Nat → Bool} {raw : Raw} {s : ServeIn}
    (h : preflight addr raw s = .error 1) :
    (∃ e, load raw = .error e) ∨
      (s.transport = some .stdio ∧ s.sessions = [] ∧
        ∃ c, load raw = .ok c ∧ c.serve.stdioSel = .configError) := by
  unfold preflight at h
  rcases bind_error h with h1 | ⟨t, ht, h⟩
  · unfold transportStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨ft, -, h⟩
  · unfold flagStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨u, -, h⟩
  · unfold countStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨u1, -, h⟩
  · unfold ledgerStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨c, hl, h⟩
  · left; unfold loadStep at h1; split at h1
    · rename_i e he; exact ⟨e, he⟩
    · simp at h1
  rcases bind_error h with h1 | ⟨ss, -, h⟩
  · right
    have hst := transportStep_ok ht
    have hl' := loadStep_ok hl
    unfold selStep at h1
    split at h1
    · simp at h1
    · simp at h1
    · rename_i hsess
      split at h1 <;> first | (simp at h1; done) | exact ⟨hst, by assumption, c, hl', by assumption⟩
    · simp at h1
  rcases bind_error h with h1 | ⟨dp, -, h⟩
  · unfold pinStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨u2, -, h⟩
  · unfold layerStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨tok, -, h⟩
  · unfold tokStep at h1; no1_tac h1
  rcases bind_error h with h1 | ⟨cs, -, h⟩
  · unfold credStep at h1; no1_tac h1
  have h1 := map_error h
  unfold bindStep at h1; no1_tac h1

/-- An unknown `--transport` is a usage error (exit 2). -/
theorem preflight_bad_transport (addr : Nat → Bool) (raw : Raw) (s : ServeIn)
    (h : s.transport = none) : preflight addr raw s = .error 2 := by
  simp [preflight, transportStep, h, bind, Except.bind]

/-- A stdio serve given several `--session`s exits 2 before the file is
read, whatever state the file is in (#32; the test
`a_stdio_serve_given_two_sessions_exits_2_before_reading_the_file`). -/
theorem preflight_stdio_many (addr : Nat → Bool) (raw : Raw) (s : ServeIn)
    (ht : s.transport = some .stdio) (hn : 1 < s.sessions.length) :
    preflight addr raw s = .error 2 := by
  simp [preflight, transportStep, flagStep, countStep, ht, hn, bind, Except.bind]

/-- A stdio serve ignores `--auth-token` and `LAMBO_AUTH_TOKEN`: neither
can refuse or change its start (#32; the test
`a_stdio_serve_ignores_a_legacy_token_it_never_uses`). -/
theorem preflight_stdio_ignores_tokens (addr : Nat → Bool) (raw : Raw) (s : ServeIn)
    (ht : s.transport = some .stdio) (fl ev : Option TokenRaw) :
    preflight addr raw { s with authFlag := fl, authEnv := ev } = preflight addr raw s := by
  simp [preflight, transportStep, flagStep, tokStep, ledgerStep, ledgerRefused, ht, bind,
    Except.bind]

/-- An invalid `--auth-token` refuses an HTTP serve (exit 2) before the
file is read, even when `LAMBO_AUTH_TOKEN`, which wins, is set and valid:
the flag is checked first, as clap checked it when `SecretToken` was its
value parser. -/
theorem preflight_bad_flag (addr : Nat → Bool) (raw : Raw) (s : ServeIn)
    (ht : s.transport = some .http) (r : TokenRaw) (hf : s.authFlag = some r) (w : Why)
    (hw : secretNew r = .error w) : preflight addr raw s = .error 2 := by
  simp [preflight, transportStep, flagStep, ht, hf, hw, bind, Except.bind]

theorem tokStep_http_some {r : TokenRaw} {ft tok : Option (List Nat)}
    (h : tokStep .http (some r) ft = .ok tok) : ∃ k, secretNew r = .ok k ∧ tok = some k := by
  simp only [tokStep] at h
  cases hr : secretNew r with
  | error w => simp [hr] at h
  | ok k => simp [hr] at h; exact ⟨k, rfl, h.symm⟩

/-- What a plan's steps establish. -/
theorem preflight_steps {addr : Nat → Bool} {raw : Raw} {s : ServeIn} {p : Plan}
    (h : preflight addr raw s = .ok p) :
    transportStep s.transport = .ok p.transport ∧ loadStep raw = .ok p.file ∧
      countStep p.transport s.sessions = .ok () ∧
      ∃ ft ss, flagStep p.transport s.authFlag = .ok ft ∧
        selStep p.transport s.sessions p.file = .ok ss ∧
        pinStep addr ss p.file p.transport = .ok (p.default, p.sessions) ∧
        layerStep p.transport p.file = .ok () ∧
        tokStep p.transport s.authEnv ft = .ok p.token ∧
        credStep p.transport p.file p.token = .ok p.creds ∧
        ledgerStep s = .ok () ∧ bindStep p.transport s.loopback p.token p.creds = .ok () := by
  unfold preflight at h
  obtain ⟨t, ht, h⟩ := bind_ok h
  obtain ⟨ft, hft, h⟩ := bind_ok h
  obtain ⟨u, hc, h⟩ := bind_ok h
  obtain ⟨u1, hlg, h⟩ := bind_ok h
  obtain ⟨c, hl, h⟩ := bind_ok h
  obtain ⟨ss, hss, h⟩ := bind_ok h
  obtain ⟨dp, hdp, h⟩ := bind_ok h
  obtain ⟨u2, hly, h⟩ := bind_ok h
  obtain ⟨tok, htk, h⟩ := bind_ok h
  obtain ⟨cs, hcs, h⟩ := bind_ok h
  obtain ⟨u3, hbd, h⟩ := map_ok h
  subst h
  exact ⟨ht, hl, hc, ft, ss, hft, hss, hdp, hly, htk, hcs, hlg, hbd⟩

/-- A set `LAMBO_AUTH_TOKEN` that is not a valid token (an exported empty
one included) never lets an HTTP serve start: no silent fall back to the
flag. -/
theorem preflight_bad_env (addr : Nat → Bool) (raw : Raw) (s : ServeIn)
    (ht : s.transport = some .http) (r : TokenRaw) (he : s.authEnv = some r) (w : Why)
    (hw : secretNew r = .error w) (p : Plan) : preflight addr raw s ≠ .ok p := by
  intro h
  obtain ⟨htr, -, -, ft, -, -, -, -, -, htk, -, -, -⟩ := preflight_steps h
  simp [transportStep, ht] at htr
  rw [← htr, he] at htk
  obtain ⟨k, hk, -⟩ := tokStep_http_some htk
  rw [hw] at hk; cases hk

/-- Over HTTP the environment wins: with `LAMBO_AUTH_TOKEN` set, the plan's
token is the environment's. -/
theorem preflight_env_wins {addr : Nat → Bool} {raw : Raw} {s : ServeIn} {p : Plan}
    (ht : s.transport = some .http) (r : TokenRaw) (he : s.authEnv = some r) {k : List Nat}
    (hk : secretNew r = .ok k) (h : preflight addr raw s = .ok p) : p.token = some k := by
  obtain ⟨htr, -, -, ft, -, -, -, -, -, htk, -, -, -⟩ := preflight_steps h
  simp [transportStep, ht] at htr
  rw [← htr, he] at htk
  obtain ⟨k', hk', h2⟩ := tokStep_http_some htk
  rw [hk] at hk'; cases hk'; exact h2

theorem pinStep_ok {addr : Nat → Bool} {ss : List Nat} {c : Config} {t : Transport}
    {x : Nat × List Nat} (h : pinStep addr ss c t = .ok x) : pinSessions addr ss c.serve t = .ok x := by
  unfold pinStep at h; split at h <;> simp_all

theorem pinSessions_http_ok {addr : Nat → Bool} {cli : List Nat} {cfg : ServeCfg} {d : Nat}
    {ss : List Nat} (h : pinSessions addr cli cfg .http = .ok (d, ss)) :
    ss = dedup (cli ++ cfg.sessions) ∧ ss.contains d = true ∧ ss.length ≤ cfg.maxAttached ∧
      (1 < ss.length → ss.all addr = true) := by
  simp only [pinSessions] at h
  split at h
  · simp at h
  · rename_i first rest hss
    split at h
    · simp at h
    · rename_i hc
      split at h
      · simp at h
      · rename_i hm
        split at h
        · simp at h
        · rename_i ha
          simp only [Except.ok.injEq, Prod.mk.injEq] at h
          obtain ⟨rfl, rfl⟩ := h
          refine ⟨rfl, by simpa using hc, by omega, fun h1 => ?_⟩
          simp only [Bool.and_eq_true, decide_eq_true_eq, not_and, Bool.not_eq_true] at ha
          simpa using ha h1

/-- A stdio plan pins exactly one session and carries no token and no
credential. -/
theorem preflight_ok_stdio {addr : Nat → Bool} {raw : Raw} {s : ServeIn} {p : Plan}
    (h : preflight addr raw s = .ok p) (ht : p.transport = .stdio) :
    s.sessions.length ≤ 1 ∧ p.sessions = [p.default] ∧ p.token = none ∧ p.creds = [] := by
  obtain ⟨-, -, hc, ft, ss, -, hss, hdp, -, htk, hcs, -, -⟩ := preflight_steps h
  rw [ht] at hc hss hdp htk hcs
  refine ⟨?_, ?_, ?_, ?_⟩
  · unfold countStep at hc; split at hc
    · simp at hc
    · rename_i hn; simp at hn; omega
  · have := pinStep_ok hdp
    simp only [pinSessions] at this
    split at this
    · simp only [Except.ok.injEq, Prod.mk.injEq] at this; obtain ⟨h1, h2⟩ := this; rw [← h2, ← h1]
    · simp at this
  · simp [tokStep] at htk; first | exact htk | exact htk.symm
  · simp [credStep] at hcs; first | exact hcs | exact hcs.symm

/-- An HTTP plan pins the ordered union of `--session` and `[serve]
sessions`, at most `max_attached` of them, all URL-addressable when there
are several, the default among them; its layers check and its credential
set passes `check_credential_set` against the token. -/
theorem preflight_ok_http {addr : Nat → Bool} {raw : Raw} {s : ServeIn} {p : Plan}
    (h : preflight addr raw s = .ok p) (ht : p.transport = .http) :
    p.sessions = dedup (s.sessions ++ p.file.serve.sessions) ∧
      p.sessions.contains p.default = true ∧
      p.sessions.length ≤ p.file.serve.maxAttached ∧
      (1 < p.sessions.length → p.sessions.all addr = true) ∧
      p.file.serve.layersOk = true ∧ credsOk p.token p.creds = true := by
  obtain ⟨-, -, -, ft, ss, -, hss, hdp, hly, -, hcs, -, -⟩ := preflight_steps h
  rw [ht] at hss hdp hly hcs
  simp only [selStep, Except.ok.injEq] at hss
  subst hss
  obtain ⟨h1, h2, h3, h4⟩ := pinSessions_http_ok (pinStep_ok hdp)
  refine ⟨h1, h2, h3, h4, ?_, ?_⟩
  · unfold layerStep at hly; split at hly
    · simp at hly
    · rename_i hn; simpa using hn
  · simp only [credStep] at hcs
    split at hcs
    · simp at hcs
    · split at hcs
      · simp_all
      · simp at hcs

/-! ## Results: what preflight leaves to `serve` -/

/-- `serve`'s own start checks (`authorize_bind`, `authorize_ledger`) never
refuse a plan `serve_preflight` passed, so for `lambo serve` an HTTP bind
beyond loopback without a credential, and `--ledger-heartbeat` without
`--ledger` or zero, are refused only in preflight, before any backend is
built (#316). -/
theorem preflight_ok_start_allowed {addr : Nat → Bool} {raw : Raw} {s : ServeIn} {p : Plan}
    (h : preflight addr raw s = .ok p) : serveStartRefused p s = false := by
  obtain ⟨-, -, -, -, -, -, -, -, -, -, -, hlg, hbd⟩ := preflight_steps h
  unfold ledgerStep at hlg
  unfold bindStep at hbd
  split at hlg
  · simp at hlg
  · split at hbd
    · simp at hbd
    · simp_all [serveStartRefused]

/-- `lambo serve --transport http` on a non-loopback bind with no credential. -/
def exposedServe : MainIn :=
  { cmd := .serve, serve := { transport := some .http, sessions := [3], loopback := false } }

/-- `lambo serve-web` on a non-loopback bind with no credential. -/
def exposedWeb : MainIn :=
  { cmd := .serveWeb, webLoopback := false }

/-- `lambo serve` and `lambo serve-web` refuse a non-loopback bind with no
credential with the same exit code, 2, before any store or embedder is built
(#316; the tests `an_exposed_bind_without_a_credential_is_refused_before_any_backend`
and `an_exposed_serve_web_bind_is_refused_before_any_backend`). -/
theorem exposed_bind_refused_before_build :
    main Features.dflt (fun _ => true) (fun _ => {}) exposedServe = ([], .exit 2) ∧
      main Features.dflt (fun _ => true) (fun _ => {}) exposedWeb = ([], .exit 2) := by
  decide

/-- `--ledger-heartbeat` without `--ledger` is refused by `serve_preflight`
with exit 2, before any store or embedder is built (#316; the test
`ledger_heartbeat_misuse_is_refused_before_any_backend`). -/
theorem heartbeat_without_ledger_refused_before_build :
    main Features.dflt (fun _ => true) (fun _ => {})
        { cmd := .serve, serve := { sessions := [3], heartbeat := some 30 } } = ([], .exit 2) := by
  decide

/-- A preflight refusal builds nothing and exits with its code. -/
theorem serve_preflight_refusal_builds_nothing (f : Features) (addr : Nat → Bool)
    (file : Nat → Raw) (m : MainIn) (hc : m.cmd = .serve) (n : Nat)
    (h : preflight addr (file 0) m.serve = .error n) : main f addr file m = ([], .exit n) := by
  simp [main, hc, h]

end Lambo.Resolve
