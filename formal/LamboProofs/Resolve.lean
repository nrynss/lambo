/-
# Command resolution, the single construction site, and serve preflight

Model of how the `lambo` binary decides what each command needs and builds
its backends, all in `src/main.rs` at `65fce2a9` (plus #315's fix to
`serve-web --auth-token`, below), with the library entry points it calls:

* `Commands::needs_embedder`, `Commands::allow_embedding_mismatch`,
  `Commands::name` (`Cmd.needsEmbedder`, `Cmd.hasMismatchFlag`, `Cmd.name`).
  There is no `needs_store` at this base: a command that does not need an
  embedder takes the store-only arm.
* `Resolved` and `resolve_for_command` (`Resolved`, `resolveForCommand`),
  with the `src/resolve.rs` entry points it calls: `resolve_backends` and
  `resolve_from_config_path` (`resolveBackends`, `resolveFull`),
  `resolve_store_only` / `resolve_store_only_read_only` (`resolveStoreOnly`,
  `resolveStoreRun`), `resolve_embedder_only` (`resolveEmbed`),
  `resolve_classification_source`, `resolve_split_io`, `resolve_verify_io`
  (`resolveClassify`, `resolveSplit`, `resolveVerify`); and below them
  `build_store_with_options` and the adapters' `vector_dimensions`
  (`buildStore`, `src/store/mod.rs`, `pg/{cockroach,postgres}.rs`,
  `sqlite/mod.rs`), `wrap_with_recall_tier` and `TieredStore`
  (`wrapTier`), `build_embedder` + `embedding_contract` (`resolveEmbedder`),
  `check_vector_compatibility` and the pin comparison (`compatCheck`,
  `widthCheck`), `check_vector_search_contract` (`claimsSearch`).
* `LamboFile::load_resolved` with `deny_unknown_fields`, the kind parsers and
  the environment overlays (`load`, `pick`, `src/config.rs`).
* `ServePlan`, `serve_preflight`, `stdio_session`, `too_many_stdio_sessions`
  (`Plan`, `preflight` and its steps), with `pin_sessions`
  (`src/mcp/serve/pinned.rs`), `plan_layers`, `resolve_auth_token`,
  `SecretToken::new` / `check_configured_token`
  (`src/surface/bearer.rs`), `check_credential_set`
  (`src/config/credential.rs`), and what `lambo::mcp::serve` refuses before
  any lease (`authorize_bind`, `authorize_ledger`: `serveStartRefused`).
* `AuthTokenArg`, `AuthTokenArgParser`, `http_auth_token_flag` and the
  `Debug` of `AuthTokenArg` / `SecretToken` (`parseAuthArg`, `render`).
* `lambo serve-web --auth-token`'s clap error: before #315's fix through
  `AuthToken: FromStr` (`serveWebFlagRefusal`, the previous version), and
  with the fix through `WebAuthTokenParser` and the shared
  `auth_token_refusal` (`parseWebAuthArg`, `serveWebFlagRefusalFixed`).
* clap's unexpected-argument error for an argv element after either
  `--auth-token`: before the fix (`flagTakesPrev`, `unexpectedArgRefusal`),
  with `allow_hyphen_values` and the first `redact_auth_token_echo` in
  a26ffd30 (`flagTakesHyphen`, `unexpectedArgRefusalFixed`, `argShown`),
  and since the re-review without `allow_hyphen_values` and with the
  combined rule (`flagTakes`, `authFlagValue`, `quotedRender`,
  `argShownFixed`).
* `fn main` from the parsed command on (`main`): the `canon` route, the
  serve and serve-web pre-reads, the resolve, the
  `unreachable!("writer override requires full backends")`, the final
  `match (cmd, resolved)` and its `_` arm ("internal resolve mismatch").
* The clap rules (`conflicts_with`, `requires`, `required_unless_present`,
  the `embed` `ArgGroup`) of `stats`, `recall`, `embed`, `derive-image`, the
  session actions, `names` and `canon alias-preview` (`rules`, `argsValid`).

## Model

* **The feature set is a parameter** (`Features`): which store and embedder
  adapters are compiled, whether the `[recall]` tier is, and the Cockroach
  migration's `VECTOR(n)` width. `Features.dflt` is the default build.
* **The file is a parameter per read**: `file i : Raw` is `lambo.toml` plus
  the environment as the `i`-th read of one resolve sees it, so the model can
  say how many times a command reads it and what happens when two reads
  differ. A `Raw` abstracts the file to what resolution decides on: unknown
  keys, how each `kind` is spelled in the file and the environment, whether
  the store is located (DSN or path), the pin, the embedder width, an
  `[embedder]` key the kind does not read, `[recall]`, `[daemon]` validity,
  and the `[serve]` table. Other overlay refusals are one flag; the order
  among load refusals is not modelled (all exit 1).
* **Construction is a trace**: each resolve returns what it built, in order,
  tagged with the `src/resolve.rs` entry point that built it (`Site`).
  SQLite and Postgres pools are lazy, so "built" means constructed, not
  connected. Not every construction refusal is modelled: the read-only
  opens of the session commands (`SqliteStore::connect_read_only` for the
  classification source and verify's split output, which refuses a missing
  file, a symlink, a hard link or a non-empty WAL) and of `stats --store`
  (`connect_existing_read_only`, which refuses a missing file) can fail
  where the model succeeds.
* **Widths are `Nat`s.** An embedder reports its configured width (the
  candle adapter refuses a `hidden_size` that disagrees at construction).
  Store construction refusals (uncompiled, unlocated, a bad width) are
  modelled; their relative order is not.
* **Session names are `Nat`s**; `addr` stands for `parse_addressed`. Stdio
  session selection from `[[serve.projects]]` is an input (`StdioSel`): path
  matching, `~` and symlinks are not modelled. Credential names `0` and `1`
  stand for the reserved `default` and `local`.
* **Tokens are byte lists** with a UTF-8 flag. Whitespace is ASCII
  whitespace; Rust's `str::trim` also trims non-ASCII whitespace, which can
  change the reason given for a refused token but never the verdict (any
  non-ASCII byte is refused).
* **Exit codes** are modelled where `main` and `serve_preflight` choose them
  (`Outcome.exit`, the `Nat` of a preflight error). A command body's own
  exit codes (`CliError::exit_code`, `canon_exit`) and `serve-web`'s planning
  (an input, `MainIn.webPlan`) are not.
* Not modelled: tracing setup, `[serve]`'s unenforced-key notice,
  `SessionBounds`, `[fusion]`/`[selection]`/calibration defaults,
  `session verify`'s `ServeConfig::validate`, `run_canon` and `canon_exit`,
  the command bodies, and clap's help and error rendering.

## Results

Loading (Level B rule 2):
* `load_refuses_unknown`, `load_unknownKey`: an unknown key, or a kind
  spelled as no adapter, in the file or the environment, refuses the load.
* `load_store_no_fallback`, `load_embed_no_fallback`: a loaded kind is the
  environment's, else the file's, else (nothing spelled) the default.

What each command needs, and the single construction site (Level B rule 4,
`level-b-pluggability.md` invariant 7, #273's second Track 1 constraint):
* `needsEmbedder_table`, `mismatchFlag_writer`, `readers_no_mismatch_flag`
  (the tests `saints_stats_inspect_provision_resolve_store_only` and
  `h1_embedding_mismatch_override_is_explicit_and_writer_only`).
* `resolve_built_allowed`: everything any resolve builds is built by the
  entry point its command's `Resolved` variant names, and is one the command
  may build; `main_built_allowed`: the model's `main` builds nothing else.
  That holds by construction (the model gives command bodies no
  construction), so for the Rust it is a modelling assumption, scoped to
  serving backends: no command body builds a store or embedder from
  `lambo.toml`. Command bodies do build SQLite stores over explicit files
  (`session split` over its `.partial` file, `canon` over its private copy),
  outside the model. Test obligation: no non-test code under `src/cli`
  calls `build_store*`, `build_embedder`, `resolve_backends`,
  `resolve_from_config_path` or `resolve_store_only*` (today such calls
  appear there only in test modules).
* `resolve_builds_at_most_one`: at most one serving store and one embedder
  per command; `resolveBackends_trace`, `resolveStoreOnly_trace`: a resolve
  stops at its first refusal.
* `embedder_only_when_needed`: a command that needs no embedder never builds
  one, except `session verify`
  (`verify_builds_embedder_though_needs_embedder_false`).
* `no_serving_store`: `embed` and the session commands build no serving
  store; `store_read_only_iff`: only `stats --store` opens it read-only
  (#248); `full_loaded_reads_none`: `serve` and `serve-web` hand their one
  read of the file to the resolve.
* `main_canon`, `resolve_canon_would_build_store`: `canon` reads no
  `lambo.toml` and builds no backend from it, and only because `main` routes
  it first (its body builds SQLite stores over its copied file, outside the
  model).

Hard errors, never a fallback (Level B rule 1):
* `full_ok`, `storeOnly_ok`, `resolveEmbed_ok`: a successful resolve hands
  the command exactly the configured kinds, compiled (embedder implemented,
  tier compiled when configured), with a positive width; so an uncompiled
  kind is a refusal (`resolveBackends_store_uncompiled`,
  `hard_error_examples`). `unknown_key_exit1`: an unknown key exits 1 for
  every command that reads the file, before anything is built.
* `storeOnly_ignores_embedder`, `embed_ignores_store`: a command resolves
  only what it needs, so a store-only command starts whatever `[embedder]`
  says, and `embed` whatever `[store]` says.

Width (store-authoritative, invariant 2):
* `full_ok`: pin and store width agree with the embedder width.
* `cockroach_width_refused`: Cockroach's `VECTOR(n)` refuses another width;
  `memory_any_dim`: the test `vector_compat_none_store_accepts_any_positive_dim`;
  `pin_mismatch_refused`: the pin check is kind-agnostic, as its comment says.
* `echo_never_width_mismatch`: for SQLite and Postgres
  `check_vector_compatibility` is an echo at resolve time and never refuses;
  the pin is the only width check that bites there. Its doc comment says so
  for SQLite only. For Postgres, the docs (`src/store/pg/mod.rs`
  `vector_dimensions`, the `src/store/pg/postgres.rs` module doc) say the
  width "is the schema's, not an unchecked echo", which holds only after
  init or attach has probed the live `vector(n)`; at resolve it is the pin,
  else the embedder width (doc mismatch filed).
* `search_contract_holds`: `check_vector_search_contract` never fires on the
  modelled adapters.

`main`:
* `resolve_ok_arm`, `main_never_mismatch_or_panic`: the final `_` arm and the
  mismatch-override `unreachable!` are unreachable.

Serve preflight:
* `preflight_exit_codes`, `preflight_exit1`: refusals exit 2, except an
  unreadable file and an undecidable `[[serve.projects]]` map (1).
* `preflight_bad_transport`; `preflight_stdio_many` (exit 2 before the file
  is read, the test `a_stdio_serve_given_two_sessions_exits_2_before_reading_the_file`);
  `preflight_stdio_ignores_tokens` (`a_stdio_serve_ignores_a_legacy_token_it_never_uses`);
  `preflight_bad_flag` (an invalid `--auth-token` refuses HTTP even when
  `LAMBO_AUTH_TOKEN` would win: the flag is checked first, as clap did);
  `preflight_bad_env` (a set but invalid variable, empty included, is never
  a fallback to the flag); `preflight_env_wins`.
* `preflight_ok_stdio`, `preflight_ok_http`: what every plan satisfies.
* `serve_preflight_refusal_builds_nothing`: a preflight refusal costs no
  backend build.

Redaction (#273's `AuthTokenArg` constraint):
* `parseAuthArg_total`: clap's value-quoting error cannot occur for
  `lambo serve --auth-token`.
* `render_noninterference`: on each of `lambo serve`'s token-only printing
  paths, and `lambo serve-web`'s flag refusal (`PrintPath`), the output
  depends on the token only through its verdict class; `debug_constant`: the
  `Debug` paths print the same for every token. This holds by definition of
  `render`, so its strength is only the completeness of `PrintPath`, which is
  a reading of the code. One path sits outside it:
  `check_serve_credentials`'s legacy-token refusal, whose output depends on
  whether the token equals a configured credential's
  (`credential_refusal_reveals_equality`, the bytes are not printed).
  `secretNew_ok`, `secretNew_examples`: the token rule.
* #315: before the fix, `lambo serve-web --auth-token`'s parser was
  `AuthToken: FromStr`, so clap's own error quoted the refused value
  (`serve_web_flag_refusal_leaks`, kept as the previous version). The fixed
  parser refuses the same tokens, hands on the same token
  (`parseWebAuthArg_ok_iff`, `parseWebAuthArg_error_iff`) and names the same
  rule (`web_flag_refusal_keeps_reason`), in `serve`'s words
  (`web_flag_refusal_eq_serve`), never the value
  (`render_noninterference` at `webFlagRefusal`;
  `serve_web_flag_refusal_fixed` on the issue's inputs).
* #315's review: clap's unexpected-argument error quoted a token starting
  with `-` (`hyphen_token_leaked`) and the tail of a token split by an
  unquoted space (`unexpected_arg_leaked`), on `serve` and `serve-web`.
  a26ffd30 took every element after the flag as its value
  (`flagTakesHyphen_total`), which was fail open (`b1_fail_open`); now a
  spaced `--auth-token` refuses a flag-like element
  (`authFlagValue_spaced_none_iff`, `flagTakes_agrees_base`),
  `--auth-token=-x` takes any value (`authFlagValue_eq_form`), and every
  quoting error prints the same for every element from the flag on
  (`quotedRender_noninterference`, `argShownFixed_from_flag`,
  `quotedRender_unexpected_is_render`, `render_noninterference` at
  `unexpectedArg`), except a missing value (`missing_value_kept`) and the
  flag's own spelling; `re_review_fixed` on the probes. What a split token
  still reveals is listed under `render_noninterference`.

Clap rules (#273's first Track 1 constraint, the rule part):
* `statsValid_iff`, `recall_rules_test`, `embedValid_iff`,
  `canon_rules_test` (the tests' cases), `serve_has_no_clap_rules`.

## Shown about the Rust (theorems about the current code)

* `bind_refused_after_build`: a non-loopback HTTP bind with no credential is
  not a preflight refusal; `serve` resolves its backends and
  `authorize_bind` then refuses with exit 1, not the usage exit 2 every
  preflight credential refusal uses.
* `heartbeat_without_ledger_after_build`: `--ledger-heartbeat`'s help says
  "Requires --ledger", but clap does not enforce it; the start is refused by
  `authorize_ledger` after the backends are built, exit 1.
* `storeOnly_kind_and_store_differ`: the store-only arm reads the file twice;
  `provision`'s `kind` and `dsn` come from the first read, its store from the
  second ("Kind comes from the same file resolve_store_only reads" holds only
  while the file does not change).
* `verify_builds_embedder_though_needs_embedder_false`.
* `session_errors_labelled_store`: a refused `session classify` or `split`
  is labelled "failed to resolve store".

## The #273 Track 1 seams

#273 proposes splitting `src/main.rs` into: a resolution dispatch module
(`resolve_for_command` + `Resolved`), a serve-bootstrap module
(`serve_preflight`, `ServePlan`, `AuthTokenArg`), per-command clap `Args`
groups, `run_canon` + `canon_exit` beside `CanonAction`, and a thin
`main.rs` router. The theorems each would carry:

* **Resolution dispatch**: `resolveForCommand` and the per-path functions
  are that module. It carries `resolve_built_allowed`,
  `resolve_builds_at_most_one`, `embedder_only_when_needed`,
  `no_serving_store`, `store_read_only_iff`, `full_ok`, `storeOnly_ok`,
  `resolve_ok_arm`, the width theorems, `full_loaded_reads_none`, and the
  pinned `storeOnly_kind_and_store_differ` and
  `verify_builds_embedder_though_needs_embedder_false`.
* **Serve bootstrap**: `preflight` and its steps. It carries every
  `preflight_*` theorem, `serve_preflight_refusal_builds_nothing`, the
  redaction theorems (`parseAuthArg_total`, `render_noninterference`,
  `debug_constant`, `web_flag_refusal_eq_serve`) and the pinned `bind_refused_after_build` and
  `heartbeat_without_ledger_after_build`.
* **Per-command arg groups**: `rules` is the contract for the rule
  attributes only (`conflicts_with`, `requires`, `required_unless_present`,
  the required `ArgGroup`); plain required args, defaults and
  `default_value_t` are not in it, so golden `--help` snapshots (`-h` and
  `--help`, every subcommand path) and parse tests carry the rest.
  `statsValid_iff`,
  `recall_rules_test`, `embedValid_iff`, `canon_rules_test`,
  `serve_has_no_clap_rules`, `mismatchFlag_writer`,
  `readers_no_mismatch_flag`.
* **Thin router** (`main`): `main_never_mismatch_or_panic`,
  `main_built_allowed` (an assumption about serving backends, see above),
  `main_canon` with `resolve_canon_would_build_store`
  (the router must keep `canon` ahead of the resolve), `unknown_key_exit1`,
  `session_errors_labelled_store`.
* **`run_canon` / `canon_exit`**: not modelled; their exit codes (0, 1 on a
  failed invariant, `CliError::exit_code` otherwise) are a test obligation.

#273's Track 1 constraints:
* *Exact clap help text*: a test obligation, and not met today. The tests
  check that help exists (`every_subcommand_and_required_arg_has_help`), that
  `provision` names its kinds and that readers do not advertise
  `--allow-embedding-mismatch`; none compares the rendered help. A golden
  snapshot of `lambo <cmd> --help` for every command, taken before the split,
  would hold it.
* *Flag names*: a test obligation (`cli_parses_help_structure`,
  `f18_no_cli_flag_accepts_a_client_timestamp`, plus the golden help); the
  model names the rule-bearing ids in `Arg`.
* *`conflicts_with` / `requires`*: `rules` with the theorems above; a test
  per command that `argsValid` cases parse or fail as listed would hold the
  rest (`embed`, `derive-image`, `names` and the session actions have no
  parse test in `main.rs` today).
* *Exit codes*: `preflight_exit_codes`, `preflight_exit1`,
  `unknown_key_exit1`, the `Outcome.exit 1` of every refused resolve, and the
  integration tests that assert them (`serve_credentials.rs`,
  `serve_multi_session.rs`, `serve_stdio_cwd_map.rs`, `cli_embed.rs`).
* *`AuthTokenArg` redaction*: `parseAuthArg_total`, `render_noninterference`,
  `debug_constant`; for `serve-web`, `parseWebAuthArg_ok_iff` and
  `web_flag_refusal_eq_serve`.
* *Single construction site*: `resolve_built_allowed` and
  `resolve_builds_at_most_one` for the resolve. That command modules build
  no backend is not proved (`main_built_allowed` holds by construction); it
  should be scoped to serving backends from `lambo.toml`, since `session
  split` and `canon` build SQLite stores over explicit files, and held by the
  source check named above.
* *Redaction* covers `lambo serve` and, since #315's fix, `lambo serve-web
  --auth-token` and clap's unexpected-argument error after either flag
  (before it, a refused value leaked through clap:
  `serve_web_flag_refusal_leaks`, `unexpected_arg_leaked`,
  `hyphen_token_leaked`). The router must keep parsing through
  `redact_auth_token_echo`.

No `f64` is involved; the model's arithmetic is `Nat` and exact.
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
is compiled (the Bedrock adapter is not implemented). -/
def EmbedderKind.implemented : EmbedderKind → Bool
  | .bedrock => false
  | _ => true

/-- The Cargo features of one build (Level B rule 1): which store and
embedder adapters are compiled in (`StoreKind::is_compiled`,
`EmbedderKind::is_compiled`), whether the `[recall]` tier is
(`recall-elastic`), and the width the Cockroach migration's `VECTOR(n)`
declares (`ddl_vector_dim`). -/
structure Features where
  store : StoreKind → Bool
  embed : EmbedderKind → Bool
  recallTier : Bool
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

theorem Cmd.mem_all (c : Cmd) : c ∈ Cmd.all := by
  cases c with
  | session a => cases a <;> decide
  | stats b => cases b <;> decide
  | _ => decide

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
SQLite store reports with no width given. -/
def defaultDim : Nat := 1024

/-- `PGVECTOR_HNSW_VECTOR_MAX_DIM`. -/
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

/-! ## Results: loading (Level B rule 2, the hard errors) -/

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

/-- No silent fallback (`level-b-pluggability.md`): a loaded store kind is
the environment's, else the file's, else (neither spells one) `memory`. -/
theorem load_store_no_fallback {r : Raw} {c : Config} (h : load r = .ok c) :
    r.storeEnv = .known c.store ∨
      (r.storeEnv = .unset ∧
        (r.storeFile = .known c.store ∨ (r.storeFile = .unset ∧ c.store = .memory))) :=
  pick_some (load_ok h).2.2.1

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

/-- The single construction site (Level B rule 4, #273's constraint): every
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

/-- `MAX_BEARER_CREDENTIAL_BYTES`. -/
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
  let c ← loadStep raw
  let ss ← selStep t s.sessions c
  let dp ← pinStep addr ss c t
  layerStep t c
  let token ← tokStep t s.authEnv flagTok
  let cs ← credStep t c token
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
or zero). Exit 1. -/
def serveStartRefused (p : Plan) (s : ServeIn) : Bool :=
  (p.transport == .http && !s.loopback && p.token.isNone && p.creds.isEmpty) ||
    (s.heartbeat.isSome && !s.ledger) || s.heartbeat == some 0

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
          | none => .ok (some c, none)
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

/-- `serve_preflight`'s refusals are usage errors (exit 2) except an
unreadable file and an undecidable `[[serve.projects]]` map (exit 1). -/
theorem preflight_exit_codes (addr : Nat → Bool) (raw : Raw) (s : ServeIn) :
    CodeOk (preflight addr raw s) := by
  unfold preflight
  refine codeOk_bind (transportStep_code _) fun t => ?_
  refine codeOk_bind (flagStep_code _ _) fun ft => ?_
  refine codeOk_bind (countStep_code _ _) fun _ => ?_
  refine codeOk_bind (loadStep_code _) fun c => ?_
  refine codeOk_bind (selStep_code _ _ _) fun ss => ?_
  refine codeOk_bind (pinStep_code _ _ _ _) fun dp => ?_
  refine codeOk_bind (layerStep_code _ _) fun _ => ?_
  refine codeOk_bind (tokStep_code _ _ _) fun tok => ?_
  refine codeOk_bind (credStep_code _ _ _) fun cs => ?_
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
  have h1 := map_error h
  unfold credStep at h1; no1_tac h1

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
  simp [preflight, transportStep, flagStep, tokStep, ht, bind, Except.bind]

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
        credStep p.transport p.file p.token = .ok p.creds := by
  unfold preflight at h
  obtain ⟨t, ht, h⟩ := bind_ok h
  obtain ⟨ft, hft, h⟩ := bind_ok h
  obtain ⟨u, hc, h⟩ := bind_ok h
  obtain ⟨c, hl, h⟩ := bind_ok h
  obtain ⟨ss, hss, h⟩ := bind_ok h
  obtain ⟨dp, hdp, h⟩ := bind_ok h
  obtain ⟨u2, hly, h⟩ := bind_ok h
  obtain ⟨tok, htk, h⟩ := bind_ok h
  obtain ⟨cs, hcs, h⟩ := map_ok h
  subst h
  exact ⟨ht, hl, hc, ft, ss, hft, hss, hdp, hly, htk, hcs⟩

/-- A set `LAMBO_AUTH_TOKEN` that is not a valid token (an exported empty
one included) never lets an HTTP serve start: no silent fall back to the
flag. -/
theorem preflight_bad_env (addr : Nat → Bool) (raw : Raw) (s : ServeIn)
    (ht : s.transport = some .http) (r : TokenRaw) (he : s.authEnv = some r) (w : Why)
    (hw : secretNew r = .error w) (p : Plan) : preflight addr raw s ≠ .ok p := by
  intro h
  obtain ⟨htr, -, -, ft, -, -, -, -, -, htk, -⟩ := preflight_steps h
  simp [transportStep, ht] at htr
  rw [← htr, he] at htk
  obtain ⟨k, hk, -⟩ := tokStep_http_some htk
  rw [hw] at hk; cases hk

/-- Over HTTP the environment wins: with `LAMBO_AUTH_TOKEN` set, the plan's
token is the environment's. -/
theorem preflight_env_wins {addr : Nat → Bool} {raw : Raw} {s : ServeIn} {p : Plan}
    (ht : s.transport = some .http) (r : TokenRaw) (he : s.authEnv = some r) {k : List Nat}
    (hk : secretNew r = .ok k) (h : preflight addr raw s = .ok p) : p.token = some k := by
  obtain ⟨htr, -, -, ft, -, -, -, -, -, htk, -⟩ := preflight_steps h
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
  obtain ⟨-, -, hc, ft, ss, -, hss, hdp, -, htk, hcs⟩ := preflight_steps h
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
  obtain ⟨-, -, -, ft, ss, -, hss, hdp, hly, -, hcs⟩ := preflight_steps h
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

/-- Shown about the Rust: a non-loopback HTTP bind with no credential is
not a preflight refusal. `serve_preflight` passes it, the backends are
resolved, and `lambo::mcp::serve`'s `authorize_bind` refuses it with exit 1
(not 2), after the store and embedder were built. The refusal still comes
before any lease. -/
def exposedServe : MainIn :=
  { cmd := .serve, serve := { transport := some .http, sessions := [3], loopback := false } }

theorem bind_refused_after_build :
    preflight (fun _ => true) {} exposedServe.serve ≠ .error 2 ∧
      main Features.dflt (fun _ => true) (fun _ => {}) exposedServe =
        ([(.backends, .store .memory false), (.backends, .embedder .bgeM3)], .exit 1) := by
  decide

/-- Shown about the Rust: `--ledger-heartbeat`'s help says "Requires
--ledger", but clap does not enforce it (no `requires`). The serve resolves
its backends and `authorize_ledger` refuses with exit 1. -/
theorem heartbeat_without_ledger_after_build :
    main Features.dflt (fun _ => true) (fun _ => {})
        { cmd := .serve, serve := { sessions := [3], heartbeat := some 30 } } =
      ([(.backends, .store .memory false), (.backends, .embedder .bgeM3)], .exit 1) := by
  decide

/-- A preflight refusal builds nothing and exits with its code. -/
theorem serve_preflight_refusal_builds_nothing (f : Features) (addr : Nat → Bool)
    (file : Nat → Raw) (m : MainIn) (hc : m.cmd = .serve) (n : Nat)
    (h : preflight addr (file 0) m.serve = .error n) : main f addr file m = ([], .exit n) := by
  simp [main, hc, h]

/-! ## `--auth-token`: the token check and redaction -/

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
flag there silently became the token (fail open, #315 re-review B1). -/
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

/-- Since #315's re-review (B1): `allow_hyphen_values` is gone again, so the
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
value (clap's missing value, S2) and the flag's own spelling (N1) are shown;
otherwise `argShown`, the narrow rule. -/
def argShownFixed (before fromFlag : List (List Nat)) (q : List Nat) : Bool :=
  q.isEmpty || q == authFlagBytes || argShown before fromFlag q

/-- The clap errors that quote an argv element, as `redact_auth_token_echo`
handles them: an unexpected argument (`InvalidArg` of `UnknownArgument`), an
invalid value of the flag `flag` (`InvalidValue`), an unrecognized
subcommand (`InvalidSubcommand`). -/
inductive Quote where
  | unexpected
  | invalidValue (flag : String)
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
piece starting `-h` prints help with exit 0); and a piece the command
accepts (`--port 80`) passes silently, leaving the weaker head as the token.
Those predate #315. -/
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
token was the flag (B1). It is removed: a spaced `--auth-token` refuses an
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

/-- A missing value keeps clap's message, with no tip (S2). -/
theorem missing_value_kept (f : String) (before fromFlag : List (List Nat)) :
    quotedRender (.invalidValue f) before fromFlag [] =
      "a value is required for '" ++ f ++ "' but none was supplied" := by
  simp [quotedRender, argShownFixed, quoteText]

set_option maxRecDepth 20000 in
/-- **The re-review's probes, fixed**: a spaced `--auth-token` refuses `-h`,
`--port=1` and `-x` and takes a lone `-`; `--auth-token=-x` takes `-x`; an
invalid `--port` value and an unrecognized subcommand after the flag are
redacted; `lambo --auth-token x serve-web` names `--auth-token`; a missing
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
      quotedRender .unexpected [] [authFlagBytes, [120]] authFlagBytes =
        "unexpected argument '--auth-token' found" ∧
      quotedRender (.invalidValue "--auth-token <TOKEN>") [] [authFlagBytes] [] =
        "a value is required for '--auth-token <TOKEN>' but none was supplied" := by
  decide

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
clap rule): the refusal comes after the backends are built
(`heartbeat_without_ledger_after_build`). -/
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
uncompiled embedder (`candle`) and an unimplemented one (`bedrock` in a
build that compiles it), each refuses `derive`; `[recall]` without
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
      (resolveForCommand { Features.dflt with embed := fun _ => true }
        (fun _ => { embedEnv := .known .bedrock }) {} .derive none).result =
        .error (.embedUnimplemented .bedrock) ∧
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
