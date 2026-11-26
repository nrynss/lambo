import LamboProofs.Resolve.Auth

/-
# Per-command clap rules (`src/main.rs`)

Models the clap rule attributes (`conflicts_with`, `requires`,
`required_unless_present`, the `embed` `ArgGroup`) of `stats`, `recall`,
`embed`, `derive-image`, the session actions, `names` and
`canon alias-preview` (`Arg`, `Rule`, `rules`, `argsValid`), and gives the
concrete instances of the construction, width and preflight results of
`Construction.lean` and `Serve.lean` on examples. These are the per-command
`Args` groups #273 proposes.

Results: `statsValid_iff`, `recall_rules_test`, `embedValid_iff` and
`canon_rules_test` (the tests' cases) state which flag combinations each
command accepts; `serve` has no clap rules (`serve_has_no_clap_rules`);
`hard_error_examples`, `construction_examples`, `width_examples` and
`preflight_examples` evaluate the headline results on concrete inputs.

Boundary: `rules` covers the rule attributes only. Plain required args,
defaults, `default_value_t`, flag names and the rendered help text are not
modelled; golden `--help` snapshots and parse tests carry them (`embed`,
`derive-image`, `names` and the session actions have no parse test in
`main.rs` today).
-/

namespace Lambo.Resolve
/-! ## Per-command argument rules (clap `conflicts_with`, `requires`, groups) -/

/-- The clap argument ids whose rules the model records. -/
inductive Arg where
  | session | store | missingVectors | json
  | query | image | mime | queryVectorJson | queryVectorAsText
  | document | origin | queriesFile
  | vectorJson
  | database | snapshot
  | apply | expectEpoch
  | similar | map | jaccard | minChars | cosine | noVectors | includeCanonical | overrides
  | maxPairs
  | ledger | ledgerHeartbeat
  deriving DecidableEq, Repr

/-- One clap rule: `conflicts_with` (symmetric), `requires`,
`required_unless_present(_any)`, and a required single-choice `ArgGroup`
(or a `conflicts_with` + `required_unless_present` pair, which is the same
rule). -/
inductive Rule where
  | conflicts (a b : Arg)
  | requires (a b : Arg)
  | requiredUnless (a : Arg) (bs : List Arg)
  | exactlyOne (as : List Arg)
  deriving DecidableEq, Repr

def Rule.holds (present : Arg → Bool) : Rule → Bool
  | .conflicts a b => !(present a && present b)
  | .requires a b => !present a || present b
  | .requiredUnless a bs => present a || bs.any present
  | .exactlyOne as => (as.filter present).length == 1

/-- The rules, per command, as `src/main.rs` declares them. `serve` has
none: `--ledger-heartbeat`'s "Requires --ledger" is help text only. -/
def rules : Cmd → List Rule
  | .stats _ =>
    [.conflicts .session .store, .requiredUnless .session [.store],
     .conflicts .store .missingVectors, .conflicts .missingVectors .store,
     .requires .missingVectors .session, .conflicts .json .session, .requires .json .store]
  | .recall =>
    [.requiredUnless .query [.image, .queryVectorJson], .conflicts .image .queryVectorJson,
     .requires .mime .image, .requires .queryVectorAsText .query,
     .requires .queryVectorAsText .queryVectorJson]
  | .embed => [.exactlyOne [.query, .document, .queriesFile], .requires .origin .document]
  | .deriveImage =>
    [.conflicts .image .vectorJson, .requiredUnless .image [.vectorJson], .requires .mime .image]
  | .session _ => [.exactlyOne [.database, .snapshot]]
  | .names => [.requires .apply .expectEpoch]
  | .canon =>
    [.conflicts .similar .map, .requires .jaccard .similar, .requires .minChars .similar,
     .requires .cosine .similar, .requires .noVectors .similar,
     .requires .includeCanonical .similar, .requires .overrides .similar,
     .requires .maxPairs .similar]
  | _ => []

def argsValid (cmd : Cmd) (present : Arg → Bool) : Bool := (rules cmd).all (·.holds present)

def statsValid (session store missing json : Bool) : Bool :=
  argsValid (.stats false) fun
    | .session => session | .store => store | .missingVectors => missing | .json => json
    | _ => false

/-- `lambo stats` takes exactly one of two shapes: `--session` with
optional `--missing-vectors`, or `--store` with optional `--json` (the test
`saints_stats_inspect_provision_resolve_store_only`). -/
theorem statsValid_iff :
    ∀ s st m j : Bool, statsValid s st m j = ((s && !st && !j) || (st && !s && !m)) := by
  decide

def recallValid (query image mime vector asText : Bool) : Bool :=
  argsValid .recall fun
    | .query => query | .image => image | .mime => mime | .queryVectorJson => vector
    | .queryVectorAsText => asText | _ => false

/-- The test `recall_query_is_optional_only_beside_an_image_or_a_vector`. -/
theorem recall_rules_test :
    recallValid false false false false false = false ∧ recallValid true false false false false ∧
      recallValid false true false false false ∧ recallValid true true true false false ∧
      recallValid false false false true false ∧ recallValid false true false true false = false ∧
      recallValid true false true false false = false ∧
      recallValid true false false true true ∧ recallValid false false false true true = false := by
  decide

def embedValid (query document file origin : Bool) : Bool :=
  argsValid .embed fun
    | .query => query | .document => document | .queriesFile => file | .origin => origin
    | _ => false

/-- `lambo embed` takes exactly one input, and `--origin` only beside
`--document`. -/
theorem embedValid_iff :
    ∀ q d fl o : Bool, embedValid q d fl o =
      (((q && !d && !fl) || (d && !q && !fl) || (fl && !q && !d)) && (!o || d)) := by
  decide

/-- `--ledger-heartbeat` without `--ledger` parses (`serve` declares no
clap rule): `serve_preflight` refuses it, exit 2, before any backend is
built (`heartbeat_without_ledger_refused_before_build`). -/
theorem serve_has_no_clap_rules : rules .serve = [] := rfl

/-- The canon alias-preview rules of the test
`canon_actions_parse_and_need_no_backend`: the `--similar` options need
`--similar`, and `--similar` excludes `--map`. -/
theorem canon_rules_test :
    argsValid .canon (fun a => [Arg.similar, .jaccard, .noVectors, .overrides].contains a) ∧
      argsValid .canon (fun a => [Arg.jaccard].contains a) = false ∧
      argsValid .canon (fun a => [Arg.overrides].contains a) = false ∧
      argsValid .canon (fun a => [Arg.similar, .map].contains a) = false := by
  decide

/-! ## Concrete instances of the headline results -/

/-- Hard errors on examples: an uncompiled store kind (`cockroach` in the
default build), an unknown key, an unknown kind in `LAMBO_STORE`, an
uncompiled embedder (`candle`) each refuses `derive`; `[recall]` without
`recall-elastic` refuses it too. -/
theorem hard_error_examples :
    (resolveForCommand Features.dflt (fun _ => { storeFile := .known .cockroach }) {} .derive
        none).result = .error (.storeUncompiled .cockroach) ∧
      (resolveForCommand Features.dflt (fun _ => { unknownKey := true }) {} .derive
        none).result = .error (.load .unknownKey) ∧
      (resolveForCommand Features.dflt (fun _ => { storeEnv := .unknown }) {} .derive
        none).result = .error (.load .unknownKind) ∧
      (resolveForCommand Features.dflt (fun _ => { embedFile := .known .candle }) {} .derive
        none).result = .error (.embedUncompiled .candle) ∧
      (resolveForCommand Features.dflt (fun _ => { tiered := true }) {} .derive
        none).result = .error .tierUncompiled := by
  decide

/-- Construction on examples: `recall` builds the store and the embedder
through `resolve_backends`; `saints` the store alone through
`resolve_store_only`; `stats --store` that store read-only; `embed` the
embedder alone. -/
theorem construction_examples :
    (resolveForCommand Features.dflt (fun _ => {}) {} .recall none).built =
        [(.backends, .store .memory false), (.backends, .embedder .bgeM3)] ∧
      (resolveForCommand Features.dflt (fun _ => {}) {} .saints none).built =
        [(.storeOnly, .store .memory false)] ∧
      (resolveForCommand Features.dflt (fun _ => {}) {} (.stats true) none).built =
        [(.storeOnly, .store .memory true)] ∧
      (resolveForCommand Features.dflt (fun _ => {}) {} .embed none).built =
        [(.embedderOnly, .embedder .bgeM3)] := by
  decide

/-- Width on examples: a SQLite store pinned to 768 under a 1024-wide
embedder is refused by the pin check; the same store unpinned echoes 1024
and resolves; Cockroach's `VECTOR(1024)` refuses a 768-wide embedder. -/
theorem width_examples :
    (resolveBackends Features.ship
        { default_config with store := .sqlite, pin := some 768 }).2 =
        .error (.pinMismatch 768 1024) ∧
      (resolveBackends Features.ship { default_config with store := .sqlite }).2 =
        .ok (.full .sqlite (some 1024) .bgeM3 1024) ∧
      (resolveBackends Features.ship
        { default_config with store := .cockroach, dim := 768 }).2 =
        .error (.widthMismatch 1024 768) := by
  decide

/-- Preflight on examples: a stdio serve with two sessions exits 2 even
over an unreadable file; with none and no default it is clap's
missing-session error (2); an undecidable map exits 1; an HTTP serve with
an exported empty `LAMBO_AUTH_TOKEN` exits 2; a stdio serve with that same
variable and a bad flag starts; HTTP pins the union in order. -/
theorem preflight_examples :
    preflight (fun _ => true) { unknownKey := true } { sessions := [1, 2] } = .error 2 ∧
      preflight (fun _ => true) {} {} = .error 2 ∧
      preflight (fun _ => true) { serve := { stdioSel := .configError } } {} = .error 1 ∧
      preflight (fun _ => true) {}
        { transport := some .http, sessions := [3], authEnv := some ⟨[], true⟩ } = .error 2 ∧
      (preflight (fun _ => true) {}
        { sessions := [3], authEnv := some ⟨[], true⟩, authFlag := some ⟨[32], true⟩ }).toBool ∧
      ((preflight (fun _ => true) { serve := { sessions := [5, 3] } }
        { transport := some .http, sessions := [3, 4] }).toOption.map (·.sessions)) =
        some [3, 4, 5] := by
  decide

end Lambo.Resolve
