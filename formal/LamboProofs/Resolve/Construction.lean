/-
# Command resolution and the single construction site

Models how the `lambo` binary decides what each command needs and builds its
backends: `Commands::needs_embedder`, `Commands::allow_embedding_mismatch` and
`Commands::name` (`Cmd.needsEmbedder`, `Cmd.hasMismatchFlag`, `Cmd.name`) in
`src/main.rs`; `Resolved` and `resolve_for_command` (`Resolved`,
`resolveForCommand`) with the `src/resolve.rs` entry points it calls:
`resolve_backends` and `resolve_from_config_path` (`resolveBackends`,
`resolveFull`), `resolve_store_only` / `resolve_store_only_read_only`
(`resolveStoreOnly`, `resolveStoreRun`), `resolve_embedder_only`
(`resolveEmbed`), `resolve_classification_source`, `resolve_split_io`,
`resolve_verify_io` (`resolveClassify`, `resolveSplit`, `resolveVerify`); below
them `build_store_with_options` and the adapters' `vector_dimensions`
(`buildStore`, `src/store/mod.rs`, `pg/{cockroach,postgres}.rs`,
`sqlite/mod.rs`), `wrap_with_recall_tier` (`wrapTier`), `build_embedder` with
`embedding_contract` (`resolveEmbedder`), `check_vector_compatibility` and the
pin comparison (`compatCheck`, `widthCheck`) and
`check_vector_search_contract` (`claimsSearch`); and `LamboFile::load_resolved`
with `deny_unknown_fields`, the kind parsers and the environment overlays
(`load`, `pick`, `src/config.rs`). The feature set is a parameter
(`Features`: which adapters and the `[recall]` tier are compiled, and the
Cockroach migration's `VECTOR(n)` width); `file i` is `lambo.toml` plus the
environment as the `i`-th read of one resolve sees it; construction is a trace
of what each resolve built, tagged with the entry point that built it (`Site`).
This is the resolution-dispatch module #273 proposes.

Results: an unknown key, or a kind spelled as no adapter, in the file or the
environment refuses the load, and a loaded kind is the environment's, else the
file's, else the default (`load_refuses_unknown`, `load_store_no_fallback`,
`load_embed_no_fallback`). Which command needs an embedder and which takes
`--allow-embedding-mismatch` is a fixed table (`needsEmbedder_table`,
`mismatchFlag_writer`, `readers_no_mismatch_flag`). Everything a resolve builds
is built by the entry point its command's `Resolved` variant names and is one
the command may build (`resolve_built_allowed`), at most one serving store and
one embedder (`resolve_builds_at_most_one`), a resolve stops at its first
refusal (`resolveBackends_trace`, `resolveStoreOnly_trace`), a command that
needs no embedder builds none except `session verify`
(`embedder_only_when_needed`), `embed` and the session commands build no
serving store and only `stats --store` opens it read-only (`no_serving_store`,
`store_read_only_iff`), and `serve` / `serve-web` hand their one read of the
file to the resolve (`full_loaded_reads_none`). A successful resolve hands the
command exactly the configured kinds, compiled, with a positive width, so an
uncompiled kind is a hard error, never a fallback (`full_ok`, `storeOnly_ok`,
`resolveEmbed_ok`, `resolveBackends_store_uncompiled`); a command resolves only
what it needs (`storeOnly_ignores_embedder`, `embed_ignores_store`). Width is
store-authoritative: pin, store and embedder widths agree on success
(`full_ok`), Cockroach's `VECTOR(n)` refuses another width
(`cockroach_width_refused`), the memory store takes any positive width
(`memory_any_dim`), the pin check is kind-agnostic (`pin_mismatch_refused`),
for SQLite and Postgres `check_vector_compatibility` is an echo at resolve time
and never refuses, so the pin is the only width check that bites there
(`echo_never_width_mismatch`; the Postgres docs' "is the schema's, not an
unchecked echo" holds only after init or attach has probed the live
`vector(n)`), and `check_vector_search_contract` never fires on the modelled
adapters (`search_contract_holds`). About the current code: the store-only arm
reads the file twice, so `provision`'s `kind` and `dsn` come from the first
read and its store from the second (`storeOnly_kind_and_store_differ`), and
`session verify` builds an embedder though `needs_embedder` is false
(`verify_builds_embedder_though_needs_embedder_false`).

Boundary: SQLite and Postgres pools are lazy, so "built" means constructed,
not connected; the read-only opens of the session commands and of
`stats --store` (`connect_read_only`, `connect_existing_read_only`, which
refuse a missing file, a symlink, a hard link or a non-empty WAL) can fail
where the model succeeds. A `Raw` abstracts the file to what resolution
decides on (other overlay refusals are one flag, and the order among load
refusals is not modelled); the relative order of store construction refusals
is not modelled; an embedder reports its configured width. That no command
body builds a serving backend from `lambo.toml` holds here by construction (the
model gives command bodies no construction), so for the Rust it is an
assumption, held by the source check that no non-test code under `src/cli`
calls `build_store*`, `build_embedder`, `resolve_backends`,
`resolve_from_config_path` or `resolve_store_only*`; `session split` and
`canon` build SQLite stores over explicit files, outside the model. Not
modelled: `[fusion]`/`[selection]`/calibration defaults, `SessionBounds` and
the command bodies. `main` and serve preflight are `Serve.lean`.
-/

namespace Lambo.Resolve

local infixl:50 " <+ " => List.Sublist

deriving instance DecidableEq for Except

/-! ## Adapter kinds and the compiled feature set -/

/-- `StoreKind` (`src/store/mod.rs`). -/
inductive StoreKind where
  | memory
  | cockroach
  | postgres
  | sqlite
  deriving DecidableEq, Repr

/-- `EmbedderKind` (`src/embed/mod.rs`). -/
inductive EmbedderKind where
  | bgeM3
  | candle
  | gemini
  | bedrock
  | fixture
  | eg2
  | elastic
  deriving DecidableEq, Repr

/-- `EmbedderKind::is_ready` with the feature taken out: every kind has an
adapter except Bedrock, whose registry arm refuses even when `embed-bedrock`
is compiled (the Bedrock adapter is not implemented).
Pinned by `formal_constants_tests::resolve`. -/
def EmbedderKind.implemented : EmbedderKind → Bool
  | .bedrock => false
  | _ => true

/-- The Cargo features of one build (Level B: features compile adapters in): which store and
embedder adapters are compiled in (`StoreKind::is_compiled`,
`EmbedderKind::is_compiled`), whether the `[recall]` tier is
(`recall-elastic`), and the width the Cockroach migration's `VECTOR(n)`
declares (`ddl_vector_dim`). -/
structure Features where
  store : StoreKind → Bool
  embed : EmbedderKind → Bool
  recallTier : Bool
  /-- Pinned by `formal_constants_tests::resolve`. -/
  crdbWidth : Nat := 1024

/-- The default feature set: `store-memory + embed-bge + embed-fixture`. -/
def Features.dflt : Features where
  store := fun | .memory => true | _ => false
  embed := fun | .bgeM3 => true | .fixture => true | _ => false
  recallTier := false

/-- A ship-like build: every store, BGE and the fixture, the recall tier. -/
def Features.ship : Features where
  store := fun _ => true
  embed := fun | .bgeM3 => true | .fixture => true | _ => false
  recallTier := true

/-! ## One read of `lambo.toml` and the environment -/

/-- How a `kind` is spelled in one source: not at all (an absent key, or an
empty environment variable, which `overlay_env` treats as unset), as a known
kind (any alias, any case), or as a string `FromStr` refuses. -/
inductive Spelled (α : Type) where
  | unset
  | known (k : α)
  | unknown
  deriving DecidableEq, Repr

/-- The effective kind: the file's spelling is parsed first (a refused
string is a TOML error), then a non-empty environment variable wins
(`LAMBO_STORE`, `LAMBO_EMBEDDER`), then the built-in default. `none` is a
hard error: an unknown spelling in either source never falls back. -/
def pick {α : Type} (file env : Spelled α) (dflt : α) : Option α :=
  match file, env with
  | .unknown, _ => none
  | _, .unknown => none
  | _, .known k => some k
  | .known k, .unset => some k
  | .unset, .unset => some dflt

/-- What `ServeConfig::select_stdio_session(None)` returns from the file
for a stdio serve given no `--session`: the `[[serve.projects]]` entry
covering the working directory, `[serve] default_session`, nothing
(`SessionSelectionError::Missing`), or a map that cannot decide
(`SessionSelectionError::Config`). Path matching is not modelled. -/
inductive StdioSel where
  | project (s : Nat)
  | fallback (s : Nat)
  | missing
  | configError
  deriving DecidableEq, Repr

/-- A raw token as the OS hands it over: its bytes, and whether they are
valid UTF-8 (`OsStr::to_str`, `OsString::into_string`). -/
structure TokenRaw where
  bytes : List Nat
  utf8 : Bool := true
  deriving DecidableEq, Repr

/-- The `[serve]` table as `serve_preflight` reads it. Session names are
`Nat`s. `creds` is `ServeConfig::resolve_credentials`: `none` when a
credential's variable is unset or not a valid token, else the
`(name, token)` pairs, names `0` and `1` standing for the reserved
`default` and `local`. -/
structure ServeCfg where
  sessions : List Nat := []
  defaultSession : Option Nat := none
  /-- `DEFAULT_MAX_ATTACHED`. Pinned by `formal_constants_tests::resolve`. -/
  maxAttached : Nat := 16
  stdioSel : StdioSel := .missing
  layersOk : Bool := true
  creds : Option (List (Nat × List Nat)) := some []
  deriving DecidableEq, Repr

/-- One read of `lambo.toml` plus the environment, before
`LamboFile::load_resolved` checks it. `unknownKey` is a key
`deny_unknown_fields` refuses (a typo like `knd` or `[embeder]`);
`overlayOk` is false when an environment overlay refuses (a DSN naming a
different database, a misplaced API-key variable, a bad
`LAMBO_PROMOTION_POLICY`); `located` says the selected store's DSN or path is
set; `pin` is `store.vector_dim`; `misappliedKey` is an `[embedder]` key the
selected kind does not read (`api_key_env`, `images`, `dimensions`, ...);
`tiered` is a `[recall]` section; `daemonValid` is `Config::validate` on the
`[daemon]` cadence. -/
structure Raw where
  unknownKey : Bool := false
  storeFile : Spelled StoreKind := .unset
  storeEnv : Spelled StoreKind := .unset
  embedFile : Spelled EmbedderKind := .unset
  embedEnv : Spelled EmbedderKind := .unset
  overlayOk : Bool := true
  located : Bool := true
  pin : Option Nat := none
  /-- The `EmbedderConfig` default width. Pinned by `formal_constants_tests::resolve`. -/
  dim : Nat := 1024
  misappliedKey : Bool := false
  tiered : Bool := false
  daemonValid : Bool := true
  serve : ServeCfg := {}
  deriving DecidableEq, Repr

/-- The resolved `LamboFile`. -/
structure Config where
  store : StoreKind
  embed : EmbedderKind
  located : Bool
  pin : Option Nat
  dim : Nat
  misappliedKey : Bool
  tiered : Bool
  daemonValid : Bool
  serve : ServeCfg
  deriving DecidableEq, Repr

/-- A witness config (the defaults). -/
def default_config : Config :=
  { store := .memory, embed := .bgeM3, located := true, pin := none, dim := 1024,
    misappliedKey := false, tiered := false, daemonValid := true, serve := {} }

inductive LoadErr where
  | unknownKey
  | unknownKind
  | overlay
  deriving DecidableEq, Repr

/-- `LamboFile::load_resolved`. -/
def load (r : Raw) : Except LoadErr Config :=
  if r.unknownKey then .error .unknownKey
  else
    match pick r.storeFile r.storeEnv .memory, pick r.embedFile r.embedEnv .bgeM3 with
    | some s, some e =>
      if r.overlayOk then
        .ok { store := s, embed := e, located := r.located, pin := r.pin, dim := r.dim,
              misappliedKey := r.misappliedKey, tiered := r.tiered,
              daemonValid := r.daemonValid, serve := r.serve }
      else .error .overlay
    | _, _ => .error .unknownKind

/-! ## Commands -/

inductive SessionAct where
  | classify
  | split
  | verify
  deriving DecidableEq, Repr

/-- `Commands` (`src/main.rs`), one constructor per subcommand. The
`--store` flag of `stats` is kept because it changes the resolve. -/
inductive Cmd where
  | session (a : SessionAct)
  | serve
  | serveWeb
  | demo
  | recall
  | embed
  | saints
  | inspect
  | stats (storeView : Bool)
  | provision
  | eraseSession
  | canon
  | names
  | recallIndex
  | derive
  | deriveImage
  | recordAction
  | reserve
  | release
  | reEmbed
  deriving DecidableEq, Repr

def Cmd.all : List Cmd :=
  [.session .classify, .session .split, .session .verify, .serve, .serveWeb, .demo,
   .recall, .embed, .saints, .inspect, .stats false, .stats true, .provision,
   .eraseSession, .canon, .names, .recallIndex, .derive, .deriveImage, .recordAction,
   .reserve, .release, .reEmbed]

/-- `Commands::needs_embedder`. -/
def Cmd.needsEmbedder : Cmd → Bool
  | .session _ | .provision | .canon | .eraseSession | .recallIndex | .names
  | .saints | .inspect | .stats _ => false
  | _ => true

/-- The variants that carry an `allow_embedding_mismatch` field, i.e. the
ones clap accepts `--allow-embedding-mismatch` on
(`Commands::allow_embedding_mismatch`). -/
def Cmd.hasMismatchFlag : Cmd → Bool
  | .serve | .demo | .derive | .deriveImage | .recordAction | .reserve | .release
  | .reEmbed => true
  | _ => false

/-- `Commands::name`. -/
def Cmd.name : Cmd → String
  | .session _ => "session"
  | .serve => "serve"
  | .serveWeb => "serve-web"
  | .demo => "demo"
  | .recall => "recall"
  | .embed => "embed"
  | .saints => "saints"
  | .inspect => "inspect"
  | .stats _ => "stats"
  | .provision => "provision"
  | .eraseSession => "erase-session"
  | .recallIndex => "recall-index"
  | .names => "names"
  | .canon => "canon"
  | .derive => "derive"
  | .deriveImage => "derive-image"
  | .recordAction => "record-action"
  | .reserve => "reserve"
  | .release => "release"
  | .reEmbed => "re-embed"

/-! ## Construction: what gets built, where, and what can refuse -/

/-- Something a resolve constructs. `store k ro` is a serving store of
kind `k` (`ro`: `StoreAccessMode::ReadOnlyExisting`), `tier` the `[recall]`
tier wrapped round it, `embedder k` an embedder. The three session objects
are SQLite stores over explicit files: the classification source over a
copied backup (`ClassificationSource::Database`), the split's destination
(`SplitDestination`) and verify's split output (`VerifySplitOutput`). -/
inductive Built where
  | store (k : StoreKind) (ro : Bool)
  | tier
  | embedder (k : EmbedderKind)
  | sourceDb
  | splitDest
  | splitOutput
  deriving DecidableEq, Repr

/-- The `src/resolve.rs` entry point that built it: `resolve_backends`,
`resolve_store_only(_read_only)`, `resolve_embedder_only`,
`resolve_classification_source`, `resolve_split_io`, `resolve_verify_io`. -/
inductive Site where
  | backends
  | storeOnly
  | embedderOnly
  | classification
  | split
  | verify
  deriving DecidableEq, Repr

def tag (s : Site) (t : List Built) : List (Site × Built) := t.map (s, ·)

/-- Why a resolve refused. Every one is a hard error: `main` prints it and
exits 1. -/
inductive Err where
  | load (e : LoadErr)
  | configNotSupported
  | verifyNeedsConfig
  | daemonInvalid
  | storeUncompiled (k : StoreKind)
  | storeUnlocated (k : StoreKind)
  | storeWidthBad
  | tierUncompiled
  | embedDimZero
  | embedMisapplied
  | embedUncompiled (k : EmbedderKind)
  | embedUnimplemented (k : EmbedderKind)
  | pinMismatch (pin dim : Nat)
  | widthMismatch (store dim : Nat)
  | sourceNeedsSqlite
  | outputNotPlain
  | importNeedsSqlite
  | outputNeedsSqlite
  | verifyNotSqlite
  | verifyHasTier
  deriving DecidableEq, Repr

/-- `Resolved` (`src/main.rs`). `full s w e d` is `ResolvedBackends` with a
store of kind `s` reporting vector width `w` and an embedder of kind `e` and
width `d`; `storeOnly kind built ro` carries the `kind` read from the first
read of the file and the kind of the store actually built, from the second. -/
inductive Resolved where
  | classification (fromDb : Bool)
  | split (fromDb dest : Bool)
  | verify (fromDb : Bool) (e : EmbedderKind) (d : Nat)
  | full (s : StoreKind) (w : Option Nat) (e : EmbedderKind) (d : Nat)
  | embedderOnly (e : EmbedderKind) (d : Nat)
  | storeOnly (kind built : StoreKind) (ro : Bool)
  deriving DecidableEq, Repr

def Resolved.isFull : Resolved → Bool
  | .full .. => true
  | _ => false

/-- `DEFAULT_POSTGRES_VECTOR_DIM`, and the `EmbedderConfig` default width a
SQLite store reports with no width given.
Pinned by `formal_constants_tests::resolve` (`DEFAULT_POSTGRES_VECTOR_DIM` with feature
`store-postgres`). -/
def defaultDim : Nat := 1024

/-- `PGVECTOR_HNSW_VECTOR_MAX_DIM`.
Pinned by `formal_constants_tests::resolve` (its Rust side with feature `store-postgres`). -/
def pgMaxWidth : Nat := 2000

/-- `Some(embedder_cfg.dim).filter(|d| *d > 0)`. -/
def posDim (c : Config) : Option Nat := if 0 < c.dim then some c.dim else none

/-- `build_store_with_options` and the adapter constructors it calls,
returning the width `vector_dimensions()` reports. `arg` is the width the
caller passes (`resolve_backends`: the embedder's; store-only: none); the
pin outranks it. -/
def buildStore (f : Features) (c : Config) (arg : Option Nat) : Except Err (Option Nat) :=
  if !f.store c.store then .error (.storeUncompiled c.store)
  else
    match c.store with
    | .memory => .ok none
    | .cockroach =>
      if c.located then .ok (some f.crdbWidth) else .error (.storeUnlocated .cockroach)
    | .postgres =>
      if !c.located then .error (.storeUnlocated .postgres)
      else
        let d := (c.pin.or arg).getD defaultDim
        if d = 0 ∨ pgMaxWidth < d then .error .storeWidthBad else .ok (some d)
    | .sqlite =>
      if !c.located then .error (.storeUnlocated .sqlite)
      else
        match c.pin.or arg with
        | some 0 => .error .storeWidthBad
        | some d => .ok (some d)
        | none => .ok (some defaultDim)

/-- `wrap_with_recall_tier`: no `[recall]` leaves the primary untouched; an
uncompiled tier is refused; a tier reports the primary's width, else the
embedder width it was handed (`TieredStore::vector_dimensions`). -/
def wrapTier (f : Features) (c : Config) (primary : Option Nat) : Except Err (Option Nat) :=
  if !c.tiered then .ok primary
  else if !f.recallTier then .error .tierUncompiled
  else .ok (primary.or (posDim c))

/-- `resolve_embedder` = `build_embedder` then `embedding_contract`. The
adapter reports the configured width (the candle adapter refuses a
`hidden_size` that disagrees at construction), so the contract check passes
whenever the build does. -/
def resolveEmbedder (f : Features) (c : Config) : Except Err Nat :=
  if c.dim = 0 then .error .embedDimZero
  else if c.misappliedKey then .error .embedMisapplied
  else if !f.embed c.embed then .error (.embedUncompiled c.embed)
  else if !c.embed.implemented then .error (.embedUnimplemented c.embed)
  else .ok c.dim

/-- `check_vector_compatibility`. -/
def compatCheck (w : Option Nat) (d : Nat) : Option Err :=
  if d = 0 then some .embedDimZero
  else
    match w with
    | some s => if s = d then none else some (.widthMismatch s d)
    | none => none

/-- The pin comparison in `resolve_backends`, then `check_vector_compatibility`. -/
def widthCheck (pin w : Option Nat) (d : Nat) : Option Err :=
  match pin with
  | some p => if p = d then compatCheck w d else some (.pinMismatch p d)
  | none => compatCheck w d

/-- Whether the built store claims `Capabilities::VECTOR_SEARCH`: every
adapter but `MemoryStore` does, and the tier adds it. -/
def claimsSearch (c : Config) : Bool :=
  c.tiered || c.store != .memory

/-- `resolve_backends`: `process_config` first (before any build), then the
store, the tier, the embedder, `check_vector_search_contract` (never fires,
`search_contract_holds`), the pin, and the width. -/
def resolveBackends (f : Features) (c : Config) : List Built × Except Err Resolved :=
  if !c.daemonValid then ([], .error .daemonInvalid)
  else
    match buildStore f c (posDim c) with
    | .error e => ([], .error e)
    | .ok w0 =>
      let t0 := [Built.store c.store false]
      match wrapTier f c w0 with
      | .error e => (t0, .error e)
      | .ok w =>
        let t1 := if c.tiered then t0 ++ [.tier] else t0
        match resolveEmbedder f c with
        | .error e => (t1, .error e)
        | .ok d =>
          let t2 := t1 ++ [.embedder c.embed]
          match widthCheck c.pin w d with
          | some e => (t2, .error e)
          | none => (t2, .ok (.full c.store w c.embed d))

/-- `resolve_store_only_with_access`: one more read of the file, the store
without a width argument, the tier. No `process_config`, no embedder, no
pin check. -/
def resolveStoreOnly (f : Features) (c : Config) (ro : Bool) :
    List Built × Except Err StoreKind :=
  match buildStore f c none with
  | .error e => ([], .error e)
  | .ok w0 =>
    let t0 := [Built.store c.store ro]
    match wrapTier f c w0 with
    | .error e => (t0, .error e)
    | .ok _ => (if c.tiered then t0 ++ [.tier] else t0, .ok c.store)

/-- `resolve_classification_source`, given which of `--database` and
`--snapshot` clap admitted (exactly one). -/
def classificationSource (f : Features) (database : Bool) : List Built × Except Err Bool :=
  if database then
    if f.store .sqlite then ([.sourceDb], .ok true) else ([], .error .sourceNeedsSqlite)
  else ([], .ok false)

/-- The invocation's inputs outside `lambo.toml`: whether `--config` was
given, and the session commands' explicit inputs (source is `--database`,
`--output-database` is a plain file path, `--dry-run`). -/
structure Inv where
  configGiven : Bool := false
  database : Bool := false
  outputPlain : Bool := true
  dryRun : Bool := false
  deriving DecidableEq, Repr

/-- One resolve: how many times it read `lambo.toml`, what it built and
where, and the result. -/
structure Run where
  reads : Nat
  built : List (Site × Built)
  result : Except Err Resolved
  deriving Repr

/-- `resolve_embedder_only` (`lambo embed`): one read, the embedder, no store. -/
def resolveEmbed (f : Features) (file : Nat → Raw) : Run :=
  match load (file 0) with
  | .error e => ⟨1, [], .error (.load e)⟩
  | .ok c =>
    match resolveEmbedder f c with
    | .error e => ⟨1, [], .error e⟩
    | .ok d => ⟨1, tag .embedderOnly [.embedder c.embed], .ok (.embedderOnly c.embed d)⟩

/-- `session classify`: refuses `--config`, reads no `lambo.toml`. -/
def resolveClassify (f : Features) (inv : Inv) : Run :=
  if inv.configGiven then ⟨0, [], .error .configNotSupported⟩
  else
    let t := (classificationSource f inv.database).1
    match (classificationSource f inv.database).2 with
    | .error e => ⟨0, tag .classification t, .error e⟩
    | .ok db => ⟨0, tag .classification t, .ok (.classification db)⟩

/-- `session split` (`resolve_split_io`): refuses `--config`, reads no
`lambo.toml`; the destination is built (with a lazy pool) whenever
`store-sqlite` is compiled, a dry run included. -/
def resolveSplit (f : Features) (inv : Inv) : Run :=
  if inv.configGiven then ⟨0, [], .error .configNotSupported⟩
  else
    let t := (classificationSource f inv.database).1
    match (classificationSource f inv.database).2 with
    | .error e => ⟨0, tag .classification t, .error e⟩
    | .ok db =>
      if !inv.outputPlain then ⟨0, tag .classification t, .error .outputNotPlain⟩
      else if !inv.dryRun && !f.store .sqlite then
        ⟨0, tag .classification t, .error .importNeedsSqlite⟩
      else
        ⟨0, tag .classification t ++ tag .split (if f.store .sqlite then [.splitDest] else []),
          .ok (.split db (f.store .sqlite))⟩

/-- `session verify` (`resolve_verify_io`): needs `--config`; reads it
once; refuses a non-SQLite `[store]` (parsed, never built) or a `[recall]`
tier; builds the source, the split output and an embedder. -/
def resolveVerify (f : Features) (file : Nat → Raw) (inv : Inv) : Run :=
  if !inv.configGiven then ⟨0, [], .error .verifyNeedsConfig⟩
  else
    match load (file 0) with
    | .error e => ⟨1, [], .error (.load e)⟩
    | .ok c =>
      if c.store != .sqlite then ⟨1, [], .error .verifyNotSqlite⟩
      else if c.tiered then ⟨1, [], .error .verifyHasTier⟩
      else if !c.daemonValid then ⟨1, [], .error .daemonInvalid⟩
      else
        let t := (classificationSource f inv.database).1
        match (classificationSource f inv.database).2 with
        | .error e => ⟨1, tag .classification t, .error e⟩
        | .ok db =>
          if !f.store .sqlite then ⟨1, tag .classification t, .error .outputNeedsSqlite⟩
          else
            match resolveEmbedder f c with
            | .error e => ⟨1, tag .classification t ++ tag .verify [.splitOutput], .error e⟩
            | .ok d =>
              ⟨1, tag .classification t ++ tag .verify [.splitOutput, .embedder c.embed],
                .ok (.verify db c.embed d)⟩

/-- The `needs_embedder()` arm: `resolve_backends(file)` on the file `serve`
or `serve-web` handed down, else `resolve_from_config_path`. -/
def resolveFull (f : Features) (file : Nat → Raw) (loaded : Option Config) : Run :=
  match loaded with
  | some c => ⟨0, tag .backends (resolveBackends f c).1, (resolveBackends f c).2⟩
  | none =>
    match load (file 0) with
    | .error e => ⟨1, [], .error (.load e)⟩
    | .ok c => ⟨1, tag .backends (resolveBackends f c).1, (resolveBackends f c).2⟩

/-- The store-only arm: `LamboFile::load_resolved` for `kind`, `dsn` and the
policy, then `resolve_store_only` (or `_read_only` for `stats --store`),
which reads the file again. -/
def resolveStoreRun (f : Features) (file : Nat → Raw) (ro : Bool) : Run :=
  match load (file 0) with
  | .error e => ⟨1, [], .error (.load e)⟩
  | .ok c0 =>
    match load (file 1) with
    | .error e => ⟨2, [], .error (.load e)⟩
    | .ok c1 =>
      let t := (resolveStoreOnly f c1 ro).1
      match (resolveStoreOnly f c1 ro).2 with
      | .error e => ⟨2, tag .storeOnly t, .error e⟩
      | .ok k => ⟨2, tag .storeOnly t, .ok (.storeOnly c0.store k ro)⟩

/-- `resolve_for_command`. `file i` is the file as the `i`-th read of this
resolve sees it; `loaded` is the file `serve` and `serve-web` already read
and hand down. -/
def resolveForCommand (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) : Run :=
  match cmd with
  | .embed => resolveEmbed f file
  | .session .split => resolveSplit f inv
  | .session .verify => resolveVerify f file inv
  | .session .classify => resolveClassify f inv
  | _ =>
    if cmd.needsEmbedder then resolveFull f file loaded
    else resolveStoreRun f file (cmd == .stats true)

/-! ## Results: loading (Level B selection, the hard errors) -/

theorem pick_unknown_file {α : Type} (env : Spelled α) (d : α) :
    pick .unknown env d = none := by
  cases env <;> rfl

theorem pick_unknown_env {α : Type} (file : Spelled α) (d : α) :
    pick file .unknown d = none := by
  cases file <;> rfl

/-- The kind `pick` settles on is the one spelled in the environment, else
the one spelled in the file, else (nothing spelled anywhere) the default. -/
theorem pick_some {α : Type} {file env : Spelled α} {d k : α} (h : pick file env d = some k) :
    env = .known k ∨ (env = .unset ∧ (file = .known k ∨ (file = .unset ∧ k = d))) := by
  cases file <;> cases env <;> simp_all [pick]

theorem load_unknownKey (r : Raw) (h : r.unknownKey = true) : load r = .error .unknownKey := by
  simp [load, h]

theorem load_ok {r : Raw} {c : Config} (h : load r = .ok c) :
    r.unknownKey = false ∧ r.overlayOk = true ∧
    pick r.storeFile r.storeEnv .memory = some c.store ∧
    pick r.embedFile r.embedEnv .bgeM3 = some c.embed ∧
    c.located = r.located ∧ c.pin = r.pin ∧ c.dim = r.dim ∧
    c.misappliedKey = r.misappliedKey ∧ c.tiered = r.tiered ∧
    c.daemonValid = r.daemonValid ∧ c.serve = r.serve := by
  unfold load at h
  split at h
  · cases h
  · split at h
    · rename_i s e hs he
      split at h
      · cases h; simp_all
      · cases h
    · cases h

/-- An unknown key, or a kind spelled as something no adapter is called,
in the file or the environment, refuses the load: never a default. -/
theorem load_refuses_unknown (r : Raw)
    (h : r.unknownKey = true ∨ r.storeFile = .unknown ∨ r.storeEnv = .unknown ∨
      r.embedFile = .unknown ∨ r.embedEnv = .unknown) :
    ∃ e, load r = .error e := by
  cases hl : load r with
  | error e => exact ⟨e, rfl⟩
  | ok c =>
    obtain ⟨hk, -, hs, he, -⟩ := load_ok hl
    rcases h with h | h | h | h | h
    · simp_all
    · rw [h, pick_unknown_file] at hs; cases hs
    · rw [h, pick_unknown_env] at hs; cases hs
    · rw [h, pick_unknown_file] at he; cases he
    · rw [h, pick_unknown_env] at he; cases he

/-- No silent fallback (Level B): a loaded store kind is
the environment's, else the file's, else (neither spells one) `memory`. -/
theorem load_store_no_fallback {r : Raw} {c : Config} (h : load r = .ok c) :
    r.storeEnv = .known c.store ∨
      (r.storeEnv = .unset ∧
        (r.storeFile = .known c.store ∨ (r.storeFile = .unset ∧ c.store = .memory))) :=
  pick_some (load_ok h).2.2.1

/-- `load_resolved` takes the embedder kind from `LAMBO_EMBEDDER`, else from
`lambo.toml`, else the default BGE-M3, and from nowhere else. -/
theorem load_embed_no_fallback {r : Raw} {c : Config} (h : load r = .ok c) :
    r.embedEnv = .known c.embed ∨
      (r.embedEnv = .unset ∧
        (r.embedFile = .known c.embed ∨ (r.embedFile = .unset ∧ c.embed = .bgeM3))) :=
  pick_some (load_ok h).2.2.2.1

/-! ## Results: what each command needs -/

/-- `--allow-embedding-mismatch` exists only on writers: commands that need
an embedder and resolve full backends (not `embed`, not the readers). This
is what makes the `unreachable!` in `main` unreachable
(`main_never_panics`). -/
theorem mismatchFlag_writer (c : Cmd) (h : c.hasMismatchFlag = true) :
    c.needsEmbedder = true ∧ c ≠ .embed ∧ c ≠ .serveWeb ∧ c ≠ .recall ∧
      ∀ a, c ≠ .session a := by
  cases c <;> simp_all [Cmd.hasMismatchFlag, Cmd.needsEmbedder]

/-- The test `h1_embedding_mismatch_override_is_explicit_and_writer_only`:
the readers do not take the override. -/
theorem readers_no_mismatch_flag :
    [Cmd.recall, .serveWeb, .saints, .inspect, .stats false, .stats true, .embed].all
      (fun c => !c.hasMismatchFlag) = true := by decide

/-- The test `saints_stats_inspect_provision_resolve_store_only` and its
neighbours: the store-only and session commands need no embedder; every
other command does. -/
theorem needsEmbedder_table :
    Cmd.all.filter (fun c => !c.needsEmbedder) =
      [.session .classify, .session .split, .session .verify, .saints, .inspect, .stats false,
       .stats true, .provision, .eraseSession, .canon, .names, .recallIndex] := by decide

/-! ## Results: the building blocks of a resolve -/

theorem buildStore_compiled {f : Features} {c : Config} {a w}
    (h : buildStore f c a = .ok w) : f.store c.store = true := by
  unfold buildStore at h
  split at h
  · cases h
  · simp_all

theorem buildStore_uncompiled {f : Features} {c : Config} (a : Option Nat)
    (h : f.store c.store = false) : buildStore f c a = .error (.storeUncompiled c.store) := by
  simp [buildStore, h]

theorem wrapTier_ok {f : Features} {c : Config} {p w} (h : wrapTier f c p = .ok w) :
    (c.tiered = false ∧ w = p) ∨ (c.tiered = true ∧ f.recallTier = true ∧ w = p.or (posDim c)) := by
  unfold wrapTier at h
  split at h
  · cases h; simp_all
  · split at h
    · cases h
    · cases h; simp_all

/-- `wrap_with_recall_tier` refuses `tiered = true` when the `recall-tier`
feature is not compiled in: a hard error, not a silent fallback. -/
theorem wrapTier_uncompiled {f : Features} {c : Config} (p : Option Nat)
    (h1 : c.tiered = true) (h2 : f.recallTier = false) :
    wrapTier f c p = .error .tierUncompiled := by
  simp [wrapTier, h1, h2]

theorem resolveEmbedder_ok {f : Features} {c : Config} {d : Nat}
    (h : resolveEmbedder f c = .ok d) :
    d = c.dim ∧ 0 < c.dim ∧ c.misappliedKey = false ∧ f.embed c.embed = true ∧
      c.embed.implemented = true := by
  unfold resolveEmbedder at h
  split at h
  · cases h
  · split at h
    · cases h
    · split at h
      · cases h
      · split at h
        · cases h
        · cases h; refine ⟨rfl, by omega, by simp_all, by simp_all, by simp_all⟩

theorem compatCheck_none {w : Option Nat} {d : Nat} (h : compatCheck w d = none) :
    0 < d ∧ (w = none ∨ w = some d) := by
  unfold compatCheck at h
  split at h
  · cases h
  · refine ⟨by omega, ?_⟩
    split at h
    · split at h
      · simp_all
      · cases h
    · simp

theorem widthCheck_none {pin w : Option Nat} {d : Nat} (h : widthCheck pin w d = none) :
    0 < d ∧ (pin = none ∨ pin = some d) ∧ (w = none ∨ w = some d) := by
  unfold widthCheck at h
  split at h
  · split at h
    · have := compatCheck_none h; subst_vars; exact ⟨this.1, by simp, this.2⟩
    · cases h
  · have := compatCheck_none h; exact ⟨this.1, by simp, this.2⟩

/-- Everything `resolve_backends` builds, in order, when it gets that far. -/
def backendsTrace (c : Config) : List Built :=
  [.store c.store false] ++ (if c.tiered then [.tier] else []) ++ [.embedder c.embed]

/-- `resolve_backends` builds a prefix of `backendsTrace`: it stops at the
first refusal and never builds anything else. -/
theorem resolveBackends_trace (f : Features) (c : Config) :
    ∃ n, (resolveBackends f c).1 = (backendsTrace c).take n := by
  cases hd : c.daemonValid
  · exact ⟨0, by simp [resolveBackends, hd]⟩
  cases hb : buildStore f c (posDim c) with
  | error e => exact ⟨0, by simp [resolveBackends, hd, hb]⟩
  | ok w0 =>
  cases hw : wrapTier f c w0 with
  | error e => exact ⟨1, by simp [resolveBackends, hd, hb, hw, backendsTrace]⟩
  | ok w =>
  cases he : resolveEmbedder f c with
  | error e =>
    exact ⟨if c.tiered then 2 else 1, by
      cases ht : c.tiered <;> simp [resolveBackends, hd, hb, hw, he, backendsTrace, ht]⟩
  | ok d =>
  exact ⟨3, by
    cases ht : c.tiered <;> cases hc : widthCheck c.pin w d <;>
      simp [resolveBackends, hd, hb, hw, he, backendsTrace, ht, hc]⟩

/-- A successful `resolve_backends` built exactly the configured store (of a
compiled kind), the tier if configured (compiled), and the configured
embedder (compiled and implemented), with a positive width that the pin and
the store's width agree with. -/
theorem resolveBackends_ok {f : Features} {c : Config} {t r}
    (h : resolveBackends f c = (t, .ok r)) :
    t = backendsTrace c ∧
      ∃ w, r = .full c.store w c.embed c.dim ∧ f.store c.store = true ∧
        (c.tiered = true → f.recallTier = true) ∧ f.embed c.embed = true ∧
        c.embed.implemented = true ∧ 0 < c.dim ∧ c.daemonValid = true ∧
        (c.pin = none ∨ c.pin = some c.dim) ∧ (w = none ∨ w = some c.dim) := by
  cases hd : c.daemonValid
  · simp [resolveBackends, hd] at h
  cases hb : buildStore f c (posDim c) with
  | error e => simp [resolveBackends, hd, hb] at h
  | ok w0 =>
  cases hw : wrapTier f c w0 with
  | error e => simp [resolveBackends, hd, hb, hw] at h
  | ok w =>
  cases he : resolveEmbedder f c with
  | error e => simp [resolveBackends, hd, hb, hw, he] at h
  | ok d =>
  cases hc : widthCheck c.pin w d with
  | some e => simp [resolveBackends, hd, hb, hw, he, hc] at h
  | none =>
  obtain ⟨rfl, -, -, hfe, himp⟩ := resolveEmbedder_ok he
  obtain ⟨hpos, hpin, hw'⟩ := widthCheck_none hc
  simp only [resolveBackends, hd, hb, hw, he, hc, Bool.not_true, Bool.false_eq_true,
    ↓reduceIte, Prod.mk.injEq, Except.ok.injEq] at h
  obtain ⟨h1, h2⟩ := h
  subst h2
  refine ⟨?_, w, rfl, buildStore_compiled hb, ?_, hfe, himp, hpos, rfl, hpin, hw'⟩
  · rw [← h1]; unfold backendsTrace; cases c.tiered <;> rfl
  · intro ht
    rcases wrapTier_ok hw with ⟨h1, -⟩ | ⟨-, h2, -⟩
    · simp_all
    · exact h2

/-- `resolve_backends` on an uncompiled store kind refuses with the
rebuild hint, before building anything. -/
theorem resolveBackends_store_uncompiled {f : Features} {c : Config}
    (hd : c.daemonValid = true) (h : f.store c.store = false) :
    resolveBackends f c = ([], .error (.storeUncompiled c.store)) := by
  simp [resolveBackends, hd, buildStore_uncompiled _ h]

/-- Everything `resolve_store_only` builds when it gets that far. -/
def storeOnlyTrace (c : Config) (ro : Bool) : List Built :=
  [.store c.store ro] ++ (if c.tiered then [.tier] else [])

theorem resolveStoreOnly_trace (f : Features) (c : Config) (ro : Bool) :
    ∃ n, (resolveStoreOnly f c ro).1 = (storeOnlyTrace c ro).take n := by
  cases hb : buildStore f c none with
  | error e => exact ⟨0, by simp [resolveStoreOnly, hb]⟩
  | ok w0 =>
  cases hw : wrapTier f c w0 with
  | error e => exact ⟨1, by simp [resolveStoreOnly, hb, hw, storeOnlyTrace]⟩
  | ok w => exact ⟨2, by cases ht : c.tiered <;> simp [resolveStoreOnly, hb, hw, storeOnlyTrace, ht]⟩

theorem resolveStoreOnly_ok {f : Features} {c : Config} {ro t k}
    (h : resolveStoreOnly f c ro = (t, .ok k)) :
    t = storeOnlyTrace c ro ∧ k = c.store ∧ f.store c.store = true ∧
      (c.tiered = true → f.recallTier = true) := by
  cases hb : buildStore f c none with
  | error e => simp [resolveStoreOnly, hb] at h
  | ok w0 =>
  cases hw : wrapTier f c w0 with
  | error e => simp [resolveStoreOnly, hb, hw] at h
  | ok w =>
  simp only [resolveStoreOnly, hb, hw, Prod.mk.injEq, Except.ok.injEq] at h
  obtain ⟨h1, h2⟩ := h
  refine ⟨?_, h2 ▸ rfl, buildStore_compiled hb, fun ht => ?_⟩
  · rw [← h1]; unfold storeOnlyTrace; cases c.tiered <;> rfl
  rcases wrapTier_ok hw with ⟨h1, -⟩ | ⟨-, h2, -⟩
  · simp_all
  · exact h2

/-! ## Results: the single construction site -/

def Cmd.isSession : Cmd → Bool
  | .session _ => true
  | _ => false

/-- The commands whose arm takes `Resolved::Full`. -/
def Cmd.isFull (c : Cmd) : Bool := c.needsEmbedder && c != .embed && !c.isSession

/-- The commands whose arm takes `Resolved::StoreOnly`. -/
def Cmd.isStoreOnly (c : Cmd) : Bool := !c.needsEmbedder && !c.isSession

/-- What a command may build, and through which `src/resolve.rs` entry
point. -/
def allowed (cmd : Cmd) : Site × Built → Bool
  | (.embedderOnly, .embedder _) => cmd == .embed
  | (.classification, .sourceDb) => cmd.isSession
  | (.split, .splitDest) => cmd == .session .split
  | (.verify, .splitOutput) => cmd == .session .verify
  | (.verify, .embedder _) => cmd == .session .verify
  | (.backends, .store _ ro) => !ro && cmd.isFull
  | (.backends, .tier) => cmd.isFull
  | (.backends, .embedder _) => cmd.isFull
  | (.storeOnly, .store _ ro) => cmd.isStoreOnly && ro == (cmd == .stats true)
  | (.storeOnly, .tier) => cmd.isStoreOnly
  | _ => false

theorem resolveForCommand_generic (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) (h1 : cmd.isSession = false) (h2 : cmd ≠ .embed) :
    resolveForCommand f file inv cmd loaded =
      if cmd.needsEmbedder then resolveFull f file loaded
      else resolveStoreRun f file (cmd == .stats true) := by
  cases cmd <;> first | rfl | exact absurd rfl h2 | simp [Cmd.isSession] at h1

theorem mem_tag {s : Site} {t : List Built} {x : Site × Built} :
    x ∈ tag s t ↔ ∃ b ∈ t, x = (s, b) := by
  simp [tag, eq_comm]

/-- `resolve_classification_source` builds the source database's store and
nothing else. -/
theorem classificationSource_built {f : Features} {d : Bool} {b : Built}
    (h : b ∈ (classificationSource f d).1) : b = .sourceDb := by
  unfold classificationSource at h
  split at h
  · split at h <;> simp_all
  · simp at h

/-- The most a resolve of `cmd` can build, whatever the file says. -/
def Shape (cmd : Cmd) (L : List (Site × Built)) : Prop :=
  match cmd with
  | .embed => ∃ k, L = [(.embedderOnly, .embedder k)]
  | .session .classify => L = [(.classification, .sourceDb)]
  | .session .split => L = [(.classification, .sourceDb), (.split, .splitDest)]
  | .session .verify =>
    ∃ k, L = [(.classification, .sourceDb), (.verify, .splitOutput), (.verify, .embedder k)]
  | _ =>
    if cmd.needsEmbedder then ∃ c, L = tag .backends (backendsTrace c)
    else ∃ c, L = tag .storeOnly (storeOnlyTrace c (cmd == .stats true))

theorem tag_take_sublist (s : Site) (l : List Built) (n : Nat) :
    tag s (l.take n) <+ tag s l :=
  (List.take_sublist n l).map _

theorem resolveFull_sublist (f : Features) (file : Nat → Raw) (loaded : Option Config) :
    ∃ c, (resolveFull f file loaded).built <+ tag .backends (backendsTrace c) := by
  unfold resolveFull
  split
  · rename_i c
    obtain ⟨n, hn⟩ := resolveBackends_trace f c
    exact ⟨c, by simp only [hn]; exact tag_take_sublist _ _ _⟩
  · split
    · exact ⟨default_config, List.nil_sublist _⟩
    · rename_i c _
      obtain ⟨n, hn⟩ := resolveBackends_trace f c
      exact ⟨c, by simp only [hn]; exact tag_take_sublist _ _ _⟩

theorem resolveStoreRun_sublist (f : Features) (file : Nat → Raw) (ro : Bool) :
    ∃ c, (resolveStoreRun f file ro).built <+ tag .storeOnly (storeOnlyTrace c ro) := by
  unfold resolveStoreRun
  split
  · exact ⟨default_config, List.nil_sublist _⟩
  · split
    · exact ⟨default_config, List.nil_sublist _⟩
    · rename_i c1 _
      obtain ⟨n, hn⟩ := resolveStoreOnly_trace f c1 ro
      refine ⟨c1, ?_⟩
      split <;> (simp only [hn]; exact tag_take_sublist _ _ _)

theorem classificationSource_fst (f : Features) (d : Bool) :
    (classificationSource f d).1 = [] ∨ (classificationSource f d).1 = [.sourceDb] := by
  unfold classificationSource
  split
  · split <;> simp
  · simp

/-- Every resolve builds at most what its command's `Shape` allows, in
order (a sublist of it). -/
theorem resolve_sublist_shape (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) :
    ∃ L, Shape cmd L ∧ (resolveForCommand f file inv cmd loaded).built <+ L := by
  by_cases hs : cmd.isSession = true
  · cases cmd <;> simp [Cmd.isSession] at hs
    rename_i a
    cases a
    · refine ⟨_, rfl, ?_⟩
      simp only [resolveForCommand, resolveClassify]
      rcases classificationSource_fst f inv.database with h | h <;>
        (repeat' split) <;> simp [tag, h]
    · refine ⟨_, rfl, ?_⟩
      simp only [resolveForCommand, resolveSplit]
      rcases classificationSource_fst f inv.database with h | h <;>
        (repeat' split) <;> simp [tag, h]
    · simp only [resolveForCommand, resolveVerify]
      cases hl : load (file 0) with
      | error e =>
        refine ⟨_, ⟨.bgeM3, rfl⟩, ?_⟩
        (repeat' split) <;> simp_all
      | ok c =>
        refine ⟨_, ⟨c.embed, rfl⟩, ?_⟩
        rcases classificationSource_fst f inv.database with h | h <;>
          (repeat' split) <;> simp_all [tag]
  by_cases he : cmd = .embed
  · subst he
    simp only [resolveForCommand, resolveEmbed]
    split
    · exact ⟨_, ⟨.bgeM3, rfl⟩, List.nil_sublist _⟩
    · rename_i c _
      split
      · exact ⟨_, ⟨.bgeM3, rfl⟩, List.nil_sublist _⟩
      · exact ⟨_, ⟨c.embed, rfl⟩, by simp [tag]⟩
  rw [resolveForCommand_generic f file inv cmd loaded (by simpa using hs) he]
  split
  · rename_i hn
    obtain ⟨c, hc⟩ := resolveFull_sublist f file loaded
    refine ⟨_, ?_, hc⟩
    cases cmd <;>
      first
        | exact ⟨c, rfl⟩
        | contradiction
        | (simp [Cmd.needsEmbedder] at hn; done)
        | (simp [Cmd.isSession] at hs; done)
  · rename_i hn
    obtain ⟨c, hc⟩ := resolveStoreRun_sublist f file (cmd == .stats true)
    refine ⟨_, ?_, hc⟩
    cases cmd <;>
      first
        | exact ⟨c, rfl⟩
        | contradiction
        | (simp [Cmd.needsEmbedder] at hn; done)
        | (simp [Cmd.isSession] at hs; done)

theorem shape_allowed {cmd : Cmd} {L : List (Site × Built)} (h : Shape cmd L) {x : Site × Built}
    (hx : x ∈ L) : allowed cmd x = true := by
  cases cmd with
  | embed => obtain ⟨k, rfl⟩ := h; simp at hx; subst hx; rfl
  | session a =>
    cases a
    · simp only [Shape] at h; subst h; simp at hx; subst hx; rfl
    · simp only [Shape] at h; subst h; simp at hx; rcases hx with rfl | rfl <;> rfl
    · obtain ⟨k, rfl⟩ := h; simp at hx; rcases hx with rfl | rfl | rfl <;> rfl
  | _ =>
    obtain ⟨c, rfl⟩ := h
    simp only [mem_tag, backendsTrace, storeOnlyTrace] at hx
    obtain ⟨b, hb, rfl⟩ := hx
    cases ht : c.tiered <;> simp [ht] at hb <;> rcases hb with rfl | rfl | rfl <;>
      first | rfl | simp [allowed, Cmd.isStoreOnly, Cmd.needsEmbedder, Cmd.isSession]

/-- The single construction site (Level B, #273's constraint): every
object any command's resolve builds comes from the `src/resolve.rs` entry
point that command's `Resolved` variant names, and is one that command may
build: a writer or reader with full backends builds through
`resolve_backends` only; a store-only command through `resolve_store_only`
only (read-only exactly for `stats --store`); `embed` an embedder through
`resolve_embedder_only` only; the session commands their explicit files. -/
theorem resolve_built_allowed (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) {x : Site × Built}
    (hx : x ∈ (resolveForCommand f file inv cmd loaded).built) : allowed cmd x = true := by
  obtain ⟨L, hL, hsub⟩ := resolve_sublist_shape f file inv cmd loaded
  exact shape_allowed hL (hsub.subset hx)

def Built.isStore : Built → Bool
  | .store .. => true
  | _ => false

def Built.isEmbedder : Built → Bool
  | .embedder _ => true
  | _ => false

theorem shape_counts {cmd : Cmd} {L : List (Site × Built)} (h : Shape cmd L) :
    L.countP (fun x => x.2.isStore) ≤ 1 ∧ L.countP (fun x => x.2.isEmbedder) ≤ 1 := by
  cases cmd with
  | embed => obtain ⟨k, rfl⟩ := h; simp [Built.isStore, Built.isEmbedder]
  | session a =>
    cases a
    · simp only [Shape] at h; subst h; decide
    · simp only [Shape] at h; subst h; decide
    · obtain ⟨k, rfl⟩ := h; simp [Built.isStore, Built.isEmbedder]
  | _ =>
    obtain ⟨c, rfl⟩ := h
    cases ht : c.tiered <;>
      simp [tag, backendsTrace, storeOnlyTrace, ht, Built.isStore, Built.isEmbedder]

/-- At most one serving store and at most one embedder per command: a
resolve never builds a second backend. -/
theorem resolve_builds_at_most_one (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) :
    (resolveForCommand f file inv cmd loaded).built.countP (fun x => x.2.isStore) ≤ 1 ∧
      (resolveForCommand f file inv cmd loaded).built.countP (fun x => x.2.isEmbedder) ≤ 1 := by
  obtain ⟨L, hL, hsub⟩ := resolve_sublist_shape f file inv cmd loaded
  have := shape_counts hL
  exact ⟨Nat.le_trans hsub.countP_le this.1, Nat.le_trans hsub.countP_le this.2⟩

/-- A command that needs no embedder never constructs one, with one
exception: `session verify`, for which `needs_embedder()` says false but
`resolve_verify_io` builds the query embedder. -/
theorem embedder_only_when_needed (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (loaded : Option Config) {s : Site} {k : EmbedderKind}
    (hx : (s, .embedder k) ∈ (resolveForCommand f file inv cmd loaded).built) :
    cmd.needsEmbedder = true ∨ cmd = .session .verify := by
  have h := resolve_built_allowed f file inv cmd loaded hx
  cases s <;> simp [allowed, Cmd.isFull] at h
  all_goals first | exact Or.inl h.1.1 | (subst h; left; rfl) | exact Or.inr h

/-- Shown about the Rust: `needs_embedder()` is false for `session verify`,
yet its resolve builds an embedder. Harmless today because
`resolve_for_command` handles the session actions before it asks
`needs_embedder()`; a router that used `needs_embedder()` to decide whether
to build an embedder would break `verify`. -/
theorem verify_builds_embedder_though_needs_embedder_false :
    Cmd.needsEmbedder (.session .verify) = false ∧
      (Site.verify, Built.embedder .bgeM3) ∈
        (resolveForCommand Features.ship (fun _ => { storeFile := .known .sqlite })
          { configGiven := true } (.session .verify) none).built := by decide

/-- `embed` and the session commands never build a serving store. -/
theorem no_serving_store {f : Features} {file : Nat → Raw} {inv : Inv} {cmd : Cmd}
    {loaded : Option Config} {s : Site} {k : StoreKind} {ro : Bool}
    (hx : (s, .store k ro) ∈ (resolveForCommand f file inv cmd loaded).built) :
    cmd ≠ .embed ∧ cmd.isSession = false := by
  have h := resolve_built_allowed f file inv cmd loaded hx
  cases s <;> simp [allowed, Cmd.isFull, Cmd.isStoreOnly] at h
  · exact ⟨h.2.1.2, by simpa using h.2.2⟩
  · refine ⟨?_, by simpa using h.1.2⟩
    rintro rfl; simp [Cmd.needsEmbedder] at h

/-- `stats --store` opens its store read-only (#248), and it is the only
command that does; every full-backend store is read-write. -/
theorem store_read_only_iff {f : Features} {file : Nat → Raw} {inv : Inv} {cmd : Cmd}
    {loaded : Option Config} {s : Site} {k : StoreKind} {ro : Bool}
    (hx : (s, .store k ro) ∈ (resolveForCommand f file inv cmd loaded).built) :
    ro = (cmd == .stats true) := by
  have h := resolve_built_allowed f file inv cmd loaded hx
  cases s <;> simp [allowed, Cmd.isFull, Cmd.isStoreOnly] at h
  · obtain ⟨rfl, hf⟩ := h
    cases cmd <;> simp_all [Cmd.needsEmbedder]
  · simpa using h.2

/-! ## Results: what a successful resolve hands each command -/

theorem resolveBackends_ok' {f : Features} {c : Config} {r : Resolved}
    (h : (resolveBackends f c).2 = .ok r) :
    ∃ w, r = .full c.store w c.embed c.dim ∧ f.store c.store = true ∧
      (c.tiered = true → f.recallTier = true) ∧ f.embed c.embed = true ∧
      c.embed.implemented = true ∧ 0 < c.dim ∧ c.daemonValid = true ∧
      (c.pin = none ∨ c.pin = some c.dim) ∧ (w = none ∨ w = some c.dim) :=
  (resolveBackends_ok (t := (resolveBackends f c).1) (Prod.ext rfl h)).2

/-- The configuration a full resolve used: the file handed down, else the
first read. -/
def fullConfig (file : Nat → Raw) (loaded : Option Config) (c : Config) : Prop :=
  loaded = some c ∨ (loaded = none ∧ load (file 0) = .ok c)

theorem resolveFull_ok {f : Features} {file : Nat → Raw} {loaded : Option Config} {r : Resolved}
    (h : (resolveFull f file loaded).result = .ok r) :
    ∃ c, fullConfig file loaded c ∧ (resolveBackends f c).2 = .ok r := by
  unfold resolveFull at h
  split at h
  · rename_i c; exact ⟨c, Or.inl rfl, h⟩
  · split at h
    · cases h
    · rename_i c hl; exact ⟨c, Or.inr ⟨rfl, hl⟩, h⟩

theorem resolveStoreRun_ok {f : Features} {file : Nat → Raw} {ro : Bool} {r : Resolved}
    (h : (resolveStoreRun f file ro).result = .ok r) :
    ∃ c0 c1, load (file 0) = .ok c0 ∧ load (file 1) = .ok c1 ∧
      r = .storeOnly c0.store c1.store ro ∧ f.store c1.store = true ∧
      (c1.tiered = true → f.recallTier = true) := by
  unfold resolveStoreRun at h
  split at h
  · cases h
  · rename_i c0 h0
    split at h
    · cases h
    · rename_i c1 h1
      split at h
      · cases h
      · rename_i k hk
        obtain ⟨-, rfl, hc, ht⟩ := resolveStoreOnly_ok (t := (resolveStoreOnly f c1 ro).1)
          (Prod.ext rfl hk)
        cases h
        exact ⟨c0, c1, h0, h1, rfl, hc, ht⟩

theorem resolveEmbed_ok {f : Features} {file : Nat → Raw} {r : Resolved}
    (h : (resolveEmbed f file).result = .ok r) :
    ∃ c, load (file 0) = .ok c ∧ r = .embedderOnly c.embed c.dim ∧ f.embed c.embed = true ∧
      c.embed.implemented = true ∧ 0 < c.dim := by
  unfold resolveEmbed at h
  split at h
  · cases h
  · rename_i c hc
    split at h
    · cases h
    · rename_i d hd
      obtain ⟨rfl, hpos, -, he, hi⟩ := resolveEmbedder_ok hd
      cases h
      exact ⟨c, hc, rfl, he, hi, hpos⟩

/-- A full-backends command that resolves gets exactly the store and the
embedder its configuration selects, both compiled, the embedder implemented,
the width positive, and pin and store width agreeing with it. So selecting
an uncompiled kind, an unbuilt tier, a zero width or a mismatched width is
never a silent fallback: the resolve does not succeed (`main` exits 1). -/
theorem full_ok {f : Features} {file : Nat → Raw} {inv : Inv} {cmd : Cmd}
    {loaded : Option Config} {r : Resolved} (hc : cmd.isFull = true)
    (h : (resolveForCommand f file inv cmd loaded).result = .ok r) :
    ∃ c w, fullConfig file loaded c ∧ r = .full c.store w c.embed c.dim ∧
      f.store c.store = true ∧ (c.tiered = true → f.recallTier = true) ∧
      f.embed c.embed = true ∧ c.embed.implemented = true ∧ 0 < c.dim ∧
      (c.pin = none ∨ c.pin = some c.dim) ∧ (w = none ∨ w = some c.dim) := by
  have hs : cmd.isSession = false := by
    cases cmd <;> simp_all [Cmd.isFull, Cmd.isSession]
  have he : cmd ≠ .embed := by rintro rfl; simp [Cmd.isFull] at hc
  have hn : cmd.needsEmbedder = true := by simp_all [Cmd.isFull]
  rw [resolveForCommand_generic f file inv cmd loaded hs he] at h
  simp only [hn, ↓reduceIte] at h
  obtain ⟨c, hcfg, hb⟩ := resolveFull_ok h
  obtain ⟨w, rfl, h1, h2, h3, h4, h5, -, h6, h7⟩ := resolveBackends_ok' hb
  exact ⟨c, w, hcfg, rfl, h1, h2, h3, h4, h5, h6, h7⟩

/-- A store-only command that resolves gets a store of a compiled kind. The
`kind` it reports (`provision` hands it to `scripts/provision.sh`) is the
first read's; the store is the second read's (`storeOnly_kind_and_store_differ`). -/
theorem storeOnly_ok {f : Features} {file : Nat → Raw} {inv : Inv} {cmd : Cmd}
    {loaded : Option Config} {r : Resolved} (hc : cmd.isStoreOnly = true)
    (h : (resolveForCommand f file inv cmd loaded).result = .ok r) :
    ∃ c0 c1, load (file 0) = .ok c0 ∧ load (file 1) = .ok c1 ∧
      r = .storeOnly c0.store c1.store (cmd == .stats true) ∧ f.store c1.store = true ∧
      (c1.tiered = true → f.recallTier = true) := by
  have hs : cmd.isSession = false := by
    cases cmd <;> simp_all [Cmd.isStoreOnly, Cmd.isSession]
  have he : cmd ≠ .embed := by rintro rfl; simp [Cmd.isStoreOnly, Cmd.needsEmbedder] at hc
  have hn : cmd.needsEmbedder = false := by simp_all [Cmd.isStoreOnly]
  rw [resolveForCommand_generic f file inv cmd loaded hs he] at h
  simp only [hn, Bool.false_eq_true, ↓reduceIte] at h
  exact resolveStoreRun_ok h

/-- The store-only path reads `lambo.toml` twice and resolves nothing
else; a store-only command builds no embedder, so it starts whatever
`[embedder]` selects, an uncompiled or unimplemented kind included, and
whatever the pin and the `[daemon]` cadence say. -/
theorem storeOnly_ignores_embedder :
    let raw : Raw := { embedFile := .known .bedrock, pin := some 7, daemonValid := false }
    (resolveForCommand Features.dflt (fun _ => raw) {} .saints none).reads = 2 ∧
      (resolveForCommand Features.dflt (fun _ => raw) {} .saints none).result =
        .ok (.storeOnly .memory .memory false) := by decide

/-- Shown about the Rust: `provision`'s `kind` and `dsn` come from the
first read of the file and its store from the second (`resolve_for_command`'s
comment says "Kind comes from the same file resolve_store_only reads"). A
file changed between the two reads hands `provision` a kind that is not the
store's. -/
theorem storeOnly_kind_and_store_differ :
    (resolveForCommand Features.ship
        (fun i => if i = 0 then { storeFile := .known .cockroach } else {}) {} .provision
        none).result = .ok (.storeOnly .cockroach .memory false) := by decide

/-- `embed` builds no store, so it starts whatever `[store]` selects. -/
theorem embed_ignores_store :
    (resolveForCommand Features.dflt (fun _ => { storeFile := .known .cockroach }) {}
        .embed none).result = .ok (.embedderOnly .bgeM3 1024) := by decide

/-- `serve` and `serve-web` hand down the file they read: the resolve reads
it no more. -/
theorem full_loaded_reads_none (f : Features) (file : Nat → Raw) (inv : Inv) (cmd : Cmd)
    (c : Config) (hc : cmd.isFull = true) :
    (resolveForCommand f file inv cmd (some c)).reads = 0 := by
  cases cmd <;> simp_all [resolveForCommand, resolveFull, Cmd.isFull, Cmd.needsEmbedder,
    Cmd.isSession]

/-! ## Results: width (store-authoritative) -/

theorem buildStore_width_echo {f : Features} {c : Config} {d : Nat} {w : Option Nat}
    (hk : c.store = .sqlite ∨ c.store = .postgres) (h : buildStore f c (some d) = .ok w) :
    w = some (c.pin.getD d) := by
  unfold buildStore at h
  rcases hk with hk | hk <;> simp only [hk] at h <;> split at h <;> try cases h
  · split at h
    · cases h
    · split at h
      · cases h
      · rename_i x hx _; cases h; cases hp : c.pin <;> simp_all
      · cases hp : c.pin <;> simp_all
  · split at h
    · cases h
    · split at h
      · cases h
      · cases h; cases hp : c.pin <;> simp_all

theorem buildStore_ne_width {f : Features} {c : Config} {a : Option Nat} {s d : Nat}
    (h : buildStore f c a = .error (.widthMismatch s d)) : False := by
  unfold buildStore at h
  split at h
  · cases h
  · split at h
    · cases h
    · split at h <;> cases h
    · split at h
      · cases h
      · dsimp only at h; split at h <;> cases h
    · split at h
      · cases h
      · split at h <;> cases h

theorem wrapTier_ne_width {f : Features} {c : Config} {p : Option Nat} {s d : Nat}
    (h : wrapTier f c p = .error (.widthMismatch s d)) : False := by
  unfold wrapTier at h
  split at h
  · cases h
  · split at h <;> cases h

theorem resolveEmbedder_ne_width {f : Features} {c : Config} {s d : Nat}
    (h : resolveEmbedder f c = .error (.widthMismatch s d)) : False := by
  unfold resolveEmbedder at h
  repeat' (first | (split at h) | cases h)

/-- Shown about the Rust (and said by its doc comment): for SQLite and Postgres the store reports
the pin, else the embedder's own width, so `check_vector_compatibility` is an
echo there and never refuses. The only width check that bites on those
kinds is the pin comparison (`pin_mismatch_refused`). -/
theorem echo_never_width_mismatch (f : Features) (c : Config)
    (hk : c.store = .sqlite ∨ c.store = .postgres) (s d : Nat) :
    (resolveBackends f c).2 ≠ .error (.widthMismatch s d) := by
  intro h
  cases hd : c.daemonValid
  · simp [resolveBackends, hd] at h
  cases hb : buildStore f c (posDim c) with
  | error e =>
    simp [resolveBackends, hd, hb] at h; subst h; exact buildStore_ne_width hb
  | ok w0 =>
  cases hw : wrapTier f c w0 with
  | error e =>
    simp [resolveBackends, hd, hb, hw] at h; subst h; exact wrapTier_ne_width hw
  | ok w =>
  cases he : resolveEmbedder f c with
  | error e =>
    simp [resolveBackends, hd, hb, hw, he] at h; subst h; exact resolveEmbedder_ne_width he
  | ok d' =>
  obtain ⟨rfl, hpos, -⟩ := resolveEmbedder_ok he
  have hp : posDim c = some c.dim := by simp [posDim, hpos]
  rw [hp] at hb
  have hw0 := buildStore_width_echo hk hb
  have hww : w = w0 := by
    rcases wrapTier_ok hw with ⟨-, rfl⟩ | ⟨-, -, rfl⟩
    · rfl
    · rw [hw0]; rfl
  subst hww hw0
  have : widthCheck c.pin (some (c.pin.getD c.dim)) c.dim ≠ some (.widthMismatch s d) := by
    cases hpn : c.pin with
    | none => simp [widthCheck, compatCheck, Nat.pos_iff_ne_zero.mp hpos]
    | some p =>
      by_cases hpd : p = c.dim
      · subst hpd; simp [widthCheck, compatCheck, Nat.pos_iff_ne_zero.mp hpos]
      · simp [widthCheck, hpd]
  cases hc : widthCheck c.pin (some (c.pin.getD c.dim)) c.dim with
  | none => simp [resolveBackends, hd, hp, hb, hw, he, hc] at h
  | some e => simp [resolveBackends, hd, hp, hb, hw, he, hc] at h; subst h; exact this hc

/-- The Cockroach width is its migration's `VECTOR(n)`, a real authority: a
configured embedder of any other width is refused. -/
theorem cockroach_width_refused (f : Features) (c : Config) (hk : c.store = .cockroach)
    (hf : f.store .cockroach = true) (hl : c.located = true) (hd : c.daemonValid = true)
    (ht : c.tiered = false) (hp : c.pin = none) (hm : c.misappliedKey = false)
    (he : f.embed c.embed = true) (hi : c.embed.implemented = true) (hpos : 0 < c.dim)
    (hne : c.dim ≠ f.crdbWidth) :
    (resolveBackends f c).2 = .error (.widthMismatch f.crdbWidth c.dim) := by
  have hne' : f.crdbWidth ≠ c.dim := Ne.symm hne
  have hz : c.dim ≠ 0 := Nat.pos_iff_ne_zero.mp hpos
  simp [resolveBackends, buildStore, wrapTier, resolveEmbedder, widthCheck, compatCheck,
    hk, hf, hl, hd, ht, hp, hm, he, hi, hz, hne']

/-- The test `vector_compat_none_store_accepts_any_positive_dim`: a
`MemoryStore` reports no width and takes any positive embedder width. -/
theorem memory_any_dim (f : Features) (c : Config) (hk : c.store = .memory)
    (hf : f.store .memory = true) (hd : c.daemonValid = true) (ht : c.tiered = false)
    (hp : c.pin = none) (hm : c.misappliedKey = false) (he : f.embed c.embed = true)
    (hi : c.embed.implemented = true) (hpos : 0 < c.dim) :
    (resolveBackends f c).2 = .ok (.full .memory none c.embed c.dim) := by
  have hz : c.dim ≠ 0 := Nat.pos_iff_ne_zero.mp hpos
  simp [resolveBackends, buildStore, wrapTier, resolveEmbedder, widthCheck, compatCheck,
    hk, hf, hd, ht, hp, hm, he, hi, hz]

/-- The pin check is kind-agnostic, so a stale pin refuses a
`memory` or `cockroach` resolve too, though neither reports the pin as its
width. -/
theorem pin_mismatch_refused (f : Features) (c : Config)
    (hk : c.store = .memory ∨ c.store = .cockroach) (hf : f.store c.store = true)
    (hl : c.located = true) (hd : c.daemonValid = true) (ht : c.tiered = false)
    (hm : c.misappliedKey = false) (he : f.embed c.embed = true)
    (hi : c.embed.implemented = true) (hpos : 0 < c.dim) (p : Nat) (hp : c.pin = some p)
    (hne : p ≠ c.dim) :
    (resolveBackends f c).2 = .error (.pinMismatch p c.dim) := by
  have hz : c.dim ≠ 0 := Nat.pos_iff_ne_zero.mp hpos
  rcases hk with hk | hk <;>
    simp [resolveBackends, buildStore, wrapTier, resolveEmbedder, widthCheck, hk, hl,
      hd, ht, hm, he, hi, hz, hp, hne] <;> simp_all

/-- `check_vector_search_contract` never fires on the modelled adapters:
once the embedder width is positive (which `resolve_backends` has checked by
then), the built store reports a width exactly when it claims
`VECTOR_SEARCH`. -/
theorem search_contract_holds {f : Features} {c : Config} {w0 w : Option Nat}
    (hpos : 0 < c.dim) (hb : buildStore f c (posDim c) = .ok w0) (hw : wrapTier f c w0 = .ok w) :
    w.isSome = claimsSearch c := by
  have hp : posDim c = some c.dim := by simp [posDim, hpos]
  rw [hp] at hb
  unfold buildStore at hb
  rcases wrapTier_ok hw with ⟨ht, rfl⟩ | ⟨ht, -, rfl⟩ <;>
    cases hk : c.store <;> simp only [hk] at hb <;> split at hb <;> (try cases hb) <;>
    (repeat' (first | (split at hb) | (cases hb))) <;> simp_all [claimsSearch]

end Lambo.Resolve
