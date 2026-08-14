# Adversarial Review: T8.3 — CLI subcommands (read + write parity)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN (R1 remediated — awaiting re-review)               ║
║  Verdict: REQUEST CHANGES — 1 P1 + 3 P2 + 7 P3                   ║
║  Remediation: all 11 findings FIXED (2026-08-14); no deferrals   ║
║  Central claims tested: readers never take the lease (HOLDS);    ║
║    writers are exactly one and always release (HOLDS);           ║
║    one construction site (HOLDS); shared validators (HOLDS);      ║
║    MCP extract behaviour-preserving (HOLDS)                      ║
║  P1: 1 → T83-1 an inverted `--parent-of` survives the whole      ║
║        suite; hierarchy direction is asserted nowhere            ║
║  P2: 3 → T83-2 `--parent-of` first-colon vs `--concept`          ║
║        last-colon silently writes wrong concepts; T83-3 the F18  ║
║        CLI guard reads only clap arg *ids*; T83-4 "readers never ║
║        spawn GC" is entirely unpinned                            ║
║  P3: 7 → tracing never initialised on the CLI path; provision    ║
║        ancestor-walk exec; readers build an unused embedder;     ║
║        Handoff integration count; indirect lease pin; reserve    ║
║        success text; saints scan drift unpinned                  ║
║  Reviewed: 593ff73 (src/ + tests/ byte-identical at review end)  ║
║  Opened: 2026-08-14                                              ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T8.3 — CLI subcommands, read + write parity with the MCP surface
(`dev-diary/PHASE-8-surface.md` §T8.3 + Handoff Log `### T8.3 — CLI subcommands (task agent,
2026-08-14)`; spec §6.2, §2.2).
**Implementing commit:** `593ff73` — `feat(P8): T8.3 CLI read+write parity with the MCP surface`.
**Scope:** `src/cli/**` (`mod.rs`, `caps.rs`, `derive.rs`, `inspect.rs`, `provision.rs`,
`recall.rs`, `record_action.rs`, `reserve.rs`, `saints.rs`, `stats.rs`), `src/main.rs`
(appends-to), `src/mcp/server.rs` (necessary shared extract), `tests/cli_write_lease.rs`,
`tests/cli_provision_sqlite.rs`.

**Method:** clause-by-clause read of every write and read verb against the ten central
properties and the seven implementor self-flags; independent re-run of all five gates;
**five source mutations** to measure what the suite actually pins (inverted `parent_of`;
divergent CLI derive content; `close()` skipped on the success path via `mem::forget`;
`Daemon::spawn()` added to reader recall; a reader taking the lease directly); a
**disguised-timestamp clap flag** to test the F18 guard; a `git diff 593ff73^..593ff73` of
`src/mcp/server.rs` and `src/main.rs` to bound the extract and the append-only rule; and a
hexdump of clap's invalid-`--kind` error to test the byte-echo posture.

**Tree state.** All five mutations were reverted by path
(`git restore --source=593ff73 -- <file>`); `src/` and `tests/` are byte-identical to `593ff73`
and all gates re-verified green afterwards. During the review another agent committed
`7984a31 docs(T8.9): …`, which moved HEAD but touches **only** `dev-diary/PHASE-8-surface.md`
(`git diff 593ff73 HEAD -- src/ tests/` is empty), so every finding below is against the exact
reviewed code. Per instruction, that markdown was left untouched. The only file this review
writes is this one.

**Gates verified independently on the clean tree:**
`cargo fmt --all -- --check` clean ·
`cargo clippy --all-targets -- -D warnings` **exit 0** ·
`cargo clippy --all-targets --features store-sqlite -- -D warnings` **exit 0** ·
`cargo test` **613 lib + 4 bin + 3 integration + 1 doctest passing, 3 ignored** ·
`cargo test --features store-sqlite` **657 lib + 4 bin + 8 integration + 1 doctest passing,
3 ignored**.

The lib, bin and ignored counts match the Handoff Log exactly (613 / 657 / 4 / 3 ignored, the
ignored being 1 lib `embed::bge_m3` live smoke + 2 integration live-calibration). The
integration counts do not — see **T83-8**.

---

## Findings

### T83-1 (P1) — An inverted `--parent-of` mapping survives the entire suite; the `Hierarchical` edge direction is asserted nowhere — CONFIRMED (mutation)

**Disposition (R1 remediation): FIXED.** Pin:
`cli::tests::parent_of_writes_hierarchical_edge_parent_to_child` — after
`derive::run --parent-of CHILD:PARENT`, `edge_between(parent, child, Hierarchical)`
(parent = right of colon). An inverted map in `derive::run` fails this test.
Shipped direction unchanged.

The shipped direction is **correct**. `parse_parent_of` returns `(parent, child)` from
`CHILD:PARENT` (`src/cli/derive.rs:22-31`), `derive::run` pushes that tuple and maps it
`(p, c)` into `ParentOf::from_pairs` (`src/cli/derive.rs:78-81`), and `graph::derive` writes
`source = parent_node, target = child_node` (`src/graph/derive.rs:383-393`). So
`--parent-of "auth middleware:user schema"` correctly makes `user schema` the parent. That
matches MCP `WireParentOf { parent, child }` and the Handoff Log.

**Nothing pins it.** Mutating the map to swap the ends:

```rust
// src/cli/derive.rs:78-81
.map(|(p, c)| (c.as_str(), p.as_str()))   // inverted
```

leaves the whole suite green:

```
cargo test --features store-sqlite
test result: ok. 657 passed; 0 failed; 1 ignored     (lib)
test result: ok. 4 passed; 0 failed                  (bin)
… all 8 integration tests ok
```

Why nothing catches it:

- `derive::tests::parent_of_child_left_parent_right` (`src/cli/derive.rs:106-111`) asserts only
  the **parser's** return values. It never reaches `ParentOf::from_pairs`, so the tuple-order
  hand-off — the actual place an inversion lives — is unguarded.
- `cli::tests::cli_mcp_differential_derive_record_recall` compares recall **hit texts**
  (`hit_contents`, `src/cli/mod.rs:226-242`). A reversed hierarchy yields the same concept set,
  so the differential is direction-blind by construction.
- `provision_then_every_subcommand_against_sqlite` passes `parent_of: vec![]`
  (`src/cli/mod.rs:493`) — the sqlite end-to-end path never exercises `--parent-of` at all.

Per the review brief ("an inverted `parent_of` is a P1 if untested") this is **P1**. It is a
test-coverage P1, not a behavioural defect: no user-visible bug exists today, but the one
directional invariant on the CLI write path is free to silently invert on any future edit.
A single assertion on `edge_between(parent, child, EdgeType::Hierarchical)` after a CLI derive
closes it.

### T83-2 (P2) — `--parent-of` splits on the FIRST colon while `--concept` splits on the LAST, so a colon-bearing concept content silently creates two wrong concepts and a wrong edge — CONFIRMED (read)

**Disposition (R1 remediation): FIXED.** `parse_parent_of` refuses more than one
colon with a usage error naming the ambiguity. Flag help says so. Not `rsplit_once`.
Pin: `derive::tests::parent_of_with_more_than_one_colon_is_usage_naming_ambiguity`
(`--parent-of "foo:bar:parent"` fails).

```22:31:src/cli/derive.rs
pub(crate) fn parse_parent_of(raw: &str) -> Result<(String, String), CliError> {
    match raw.split_once(':') {
        Some((child, parent)) if !child.trim().is_empty() && !parent.trim().is_empty() => {
            Ok((parent.to_string(), child.to_string()))
```

`parse_concept` uses `rsplit_once` and the committed test
`concept_splits_on_last_colon` proves colon-bearing contents are **legal and supported**
(`parse_concept("foo:bar:entity")` → content `"foo:bar"`). `--parent-of` cannot express the
same content: `--parent-of "foo:bar:parent"` yields `child = "foo"`, `parent = "bar:parent"`.

This does not error. `graph::derive` creates unknown `parent_of` ends as new `Entity` concepts
(`PARENT_OF_CONCEPT_TYPE`, `src/graph/derive.rs:110-114`), so the command **succeeds** and
silently writes two junk concepts (`foo`, `bar:parent`) plus a `Hierarchical` edge between
them, while the concepts the operator meant are untouched. Silent wrong writes are the worst
class of surface bug, and the asymmetry with `--concept`'s documented last-colon rule makes it
a trap rather than a limitation.

`rsplit_once` is not the fix — with `CHILD:PARENT` neither side is a closed token, so any
single-colon syntax is genuinely ambiguous. The cheap fix is to refuse `--parent-of` values
containing more than one colon with a usage error naming the ambiguity, and to say so in the
flag help.

### T83-3 (P2) — The F18 CLI guard inspects only clap arg *ids*, never `get_long()`, so a literal `--occurred-at` flag passes — CONFIRMED (mutation)

**Disposition (R1 remediation): FIXED.** `f18_no_cli_flag_accepts_a_client_timestamp`
now walks `get_id()`, `get_long()`, and `get_all_aliases()`, and matches banned
tokens as substrings. A `#[arg(long = "occurred-at")]` mutant fails. Shipped flags
stay F18-clean.

```578:586:src/main.rs
        fn walk(cmd: &clap::Command) {
            for arg in cmd.get_arguments() {
                let id = arg.get_id().as_str().to_lowercase();
                let id = id.replace('-', "_");
                assert!(
                    !BANNED.contains(&id.as_str()),
                    "F18: CLI flag '{id}' looks like a client timestamp"
                );
```

clap's arg id defaults to the field name and is **not** changed by `long = "…"`. This file
already uses divergent longs in four places (`long = "parent-of"`, `long = "depends-on"`,
`long = "ttl-seconds"`, `long = "max-tokens"`), so the divergence is an established local
pattern, not a hypothetical.

Mutation — added to the `Derive` variant:

```rust
#[arg(long = "occurred-at", help = "MUTANT probe client clock.")]
stamp: Option<String>,
```

`--occurred-at` is one of the `BANNED` strings verbatim. Result:

```
running 4 tests
test tests::f18_no_cli_flag_accepts_a_client_timestamp ... ok
test tests::every_subcommand_and_required_arg_has_help ... ok
test result: ok. 4 passed; 0 failed
```

The guard passed a flag it exists to forbid. This is the CLI twin of **T82-4** (the MCP F18
guard that walked only top-level schema properties) and deserves the same treatment: also walk
`arg.get_long()` and `arg.get_all_aliases()`, and match on substrings rather than whole-token
equality (`--start-time` passes the current list too). Two lines.

Note the *shipped* flag set is genuinely F18-clean — I found no real timestamp flag. Only the
guard is weak.

### T83-4 (P2) — "Readers never `Daemon::spawn()` (spawn = GC = writer)" is entirely unpinned — CONFIRMED (mutation)

**Disposition (R1 remediation): FIXED.** Pin:
`cli::tests::reader_recall_does_not_spawn_gc_or_mutate_epoch` — production
`recall.rs` must not contain `.spawn()` / `Daemon::spawn`; after a reader
`recall`, graph epoch and canonization statuses are unchanged. Adding
`daemon.spawn()` in `recall.rs` fails this test.

The code is right, and the reason is documented in two places
(`src/cli/recall.rs:71-73`, module docs at `src/cli/recall.rs:1-5`). But adding the spawn back:

```rust
// src/cli/recall.rs:73
let daemon = Daemon::from_config(loaded.graph, &cfg).with_index(loaded.index);
let _gc = daemon.spawn();   // GC in a reader process
```

leaves everything green — 657 lib + 4 bin + all 8 integration tests pass. A reader silently
running garbage collection and canonization against a session a `serve` writer owns is exactly
the concurrent-mutation hazard §2.2 exists to prevent, and it would ship unnoticed.

Same class as T83-1 (a correct implementation with no guard). It is cheap to pin from the
outside: after a reader `recall` on a session with GC-eligible state, assert the graph epoch
and canonization statuses are unchanged, or assert `daemon.events()` produced nothing.

### T83-5 (P3) — No tracing subscriber is ever installed on the CLI path, so every `tracing::warn!` a CLI command emits is discarded — CONFIRMED (read)

**Disposition (R1 remediation): FIXED.** Embed-failure and daemon `RecallResult.warnings`
are prepended as `⚑` lines on recall stdout (`render_recall_text`). Tracing is still
not installed for every CLI invocation. Pin:
`cli::tests::recall_prints_skipped_vector_leg_when_embed_fails`.

`lambo::mcp::init_tracing()` is called **only** in the `Serve` arm (`src/main.rs:339`).
`run_async` (`src/main.rs:276-300`) installs nothing, so for all nine other subcommands
`tracing` events are no-ops. Two warnings are lost that an operator needs:

- `src/cli/recall.rs:82-89` — `"recall: query embedding failed; vector leg skipped"`. A
  `lambo recall` against a `VECTOR_SEARCH` store whose embedder is down **silently degrades to
  keyword-only** and prints a normal-looking context block. Nothing on stdout or stderr says
  the semantic leg was skipped.
- `caps::clamp_cfg_default` (`src/cli/caps.rs:119-130`) — the "config default outside the
  surface bound" notice never appears.

The recall degradation is the substantive half: a quiet quality regression in the primary read
verb is indistinguishable from a correct answer. Cheapest honest fix is to surface the skipped
vector leg in the returned text (the surface already carries `⚑` warning lines), rather than
initialising tracing for every CLI invocation.

### T83-6 (P3) — `provision` walks cwd and every ancestor to `/` looking for `scripts/provision.sh` and executes it with `bash`, with no repo-root marker — CONFIRMED (read)

**Disposition (R1 remediation): FIXED.** Require a Cargo.toml whose `[package] name`
is `lambo` beside `scripts/provision.sh`; bound the walk at 16 ancestors; echo the
resolved path on stderr before `bash`. Pins:
`provision::marker_tests::cargo_toml_marker_requires_package_name_lambo`,
`provision::marker_tests::provision_script_without_lambo_marker_is_ignored`.

```59:71:src/cli/provision.rs
fn find_provision_script() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("scripts").join("provision.sh");
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
}
```

`Command::new("bash").arg(&script).status()` then runs whatever it found
(`src/cli/provision.rs:38`). Nothing checks that the directory is the lambo repo (no
`Cargo.toml` / package-name check) and the walk is unbounded, so running
`lambo provision` from any subdirectory of a world- or group-writable ancestor that contains
`scripts/provision.sh` executes that script with the operator's privileges. On a shared host
(`/tmp/<something>/…`, a shared `/opt` tree) that is a realistic local escalation vector.

I did **not** demonstrate this end-to-end: the cockroach arm is only reached after
`resolve_for_command` successfully builds a Cockroach store, which needs the `store-cockroach`
feature plus a reachable DSN — neither available here. The path is unambiguous on reading.
Mitigation is cheap: require a repo marker beside `scripts/` (a `Cargo.toml` naming `lambo`),
or bound the walk, and print the resolved path before executing.

On the second half of the same probe — **no DSN or path leak was found.** `init_schema`
failures are wrapped as `format!("init_schema: {e}")` (`src/cli/provision.rs:27`) and reach
only stderr, an operator stream, never a model-facing string; the T8.2 `redact_urls` posture
applies to MCP warnings and is not required here.

### T83-7 (P3) — Readers resolve a full `ResolvedBackends`, constructing an embedder that `saints` / `stats` / `inspect` never use — CONFIRMED (read)

**Disposition (R1 remediation): FIXED.** `needs_embedder()` is false for `saints` /
`stats` / `inspect` / `provision`; those three take `Resolved::StoreOnly` and
`run(&dyn GraphStore, …)` like provision. No `Resolved` redesign. Pin:
`tests::saints_stats_inspect_provision_resolve_store_only`.

`needs_embedder()` excludes only `Provision` (`src/main.rs:238-240`), so `saints`, `stats` and
`inspect` all take the `Resolved::Full` path and build an embedder none of them touch — only
`recall` embeds, and then only under `Capabilities::VECTOR_SEARCH`.

Today this is nearly free: `BgeM3LlamaCppEmbedder::new` performs no I/O (it validates the URL
and builds a client, `src/embed/bge_m3.rs:71-92`), so no reader needs llama.cpp running. The
defect is the coupling, not the cost — `build_embedder` **can** hard-fail on config alone. With
`embedder.kind = "bedrock"` it returns `Err("embed-bedrock is enabled but BedrockEmbedder is
not implemented yet (T7.1)")` (`src/embed/mod.rs:268-276`), which makes `lambo stats` — a
lease-free read of durable counts — fail on an embedder it would never have called. That
undercuts the "readers always answer" posture §2.2 sets up.

Fix is a third `Resolved` variant (or reusing `StoreOnly`) for the three non-embedding readers.
Slightly more than a one-liner, hence P3 rather than P2.

### T83-8 (P3) — The Handoff Log's `store-sqlite` integration test count is understated, and the two feature sets are counted by different conventions — CONFIRMED (re-run)

**Disposition (R1 remediation): FIXED.** T8.3 task-agent Handoff Log now uses one
convention: default **3 integration + 1 doctest**, sqlite **8 integration + 1
doctest** (not 7).

Claimed (`dev-diary/PHASE-8-surface.md`, T8.3 Handoff Log): "**613 lib + 4 bin + 4
integration** passing, 3 ignored" and "**657 lib + 4 bin + 7 integration** passing, 3 ignored".

Measured:

| Feature set | lib | bin | integration tests | doctest | ignored |
|---|---|---|---|---|---|
| default | 613 ✓ | 4 ✓ | **3** | 1 | 3 ✓ |
| `store-sqlite` | 657 ✓ | 4 ✓ | **8** | 1 | 3 ✓ |

The default-feature "4" is only reachable as 3 integration + 1 doctest. Under that same
convention the sqlite line should read **9**, not 7; counting integration tests alone it is
**8** (`cli_provision_sqlite` 1, `cli_write_lease` 1, `p2_integration` 2, `rebuild_session` 1,
`serve_pre_handshake_durability` 1, `serve_sigterm_durability` 1, `serve_single_writer_lease`
1). No count is inflated and nothing is missing — the numbers understate the evidence — but a
handoff whose two headline lines use different counting rules is not a baseline the next agent
can diff against. lib, bin and ignored counts are exact.

### T83-9 (P3) — The lease-release property is pinned only indirectly, via a 20-second timeout whose failure message never mentions the lease — CONFIRMED (mutation)

**Disposition (R1 remediation): FIXED.** After the no-serve derive,
`tests/cli_write_lease.rs` asserts a second writer derive succeeds — the lease was
released — with a failure message that names the property.

Good news first: the property **is** pinned (see Verified holds). Skipping `close()` on the
success path is caught. But this is how it is caught:

```
---- derive_succeeds_with_no_serve_and_fails_closed_while_serve_holds stdout ----
panicked at tests/cli_write_lease.rs:30:33:
no JSON-RPC frame with id 1 within 20s: channel is empty and sending half is closed
```

The real cause was visible only in the inherited stderr (`lambo serve: conflict: session
t8.3-cli-lease is already held by another writer (agent-free@…)`). The test detects a leaked
lease as a **side effect** of `serve` then failing to start, pays 20 seconds of wall clock to
do it, and reports something that reads like a transport flake. A future agent debugging that
message will look at JSON-RPC framing, not at `close_writer`.

One explicit assertion after the no-serve derive — that the session lease row is absent, or
that a second CLI derive immediately succeeds — pins the same property in milliseconds and
names it.

### T83-10 (P3) — `reserve`'s success text prints an `until <timestamp>` that is void microseconds later — CONFIRMED (read); escalation of self-flag 1

**Disposition (R1 remediation): FIXED.** Success text (and clap help) say the
reservation ends when this process exits. The TTL/`expires_at` is labelled as the
value that would apply inside a long-lived writer such as `serve`, not a CLI hold.
Pin: sqlite `provision_then_every_subcommand_against_sqlite` asserts
`this process exits` and not `lost on restart`.

```56:63:src/cli/reserve.rs
            let summary = format!(
                "reserved {} until {} for agent '{}'\n\
                 reservations are advisory and RAM-local: they are lost on restart",
                node_id.0,
                reservation.expires_at.to_rfc3339(),
```

The implementor's self-flag 1 is honest about the *design*, and I agree with the design (see
ruling below). The **message** is not honest enough. It prints a concrete future expiry (up to
`--ttl-seconds 3600` away) and attributes the loss to "restart", when in fact `close_writer`
runs on the next line and the reservation is gone before the operator's shell prompt returns.
An operator reading `reserved … until 15:47:31` reasonably concludes they hold a lock for the
next hour; they hold nothing.

The text should say the reservation ends when this process exits — i.e. now — and either drop
the `until` timestamp or label it as the TTL that *would* have applied inside a long-lived
writer such as `serve`. Text-only change.

### T83-11 (P3) — The copied `canonical_memories` scan has no differential guard, so CLI `saints` and MCP `lambo_saints` can silently disagree — CONFIRMED (read); escalation of self-flag 3

**Disposition (R1 remediation): FIXED.** Pin:
`cli::saints::parity::canonical_memories_from_graph_agrees_with_memory` —
`canonical_memories_from_graph` equals `Memory::canonical_memories` on one shared
graph.

`cli::saints::canonical_memories_from_graph` (`src/cli/saints.rs:41-62`) reimplements
`Memory::canonical_memories` — same Canonical filter, same blast-radius-desc / `created_at` /
`node_id` sort. The implementor flagged the duplication and correctly notes `memory.rs` is out
of `owns`.

What is missing is the cheap half: **no test compares the two.** The differential test covers
derive / record-action / recall only; `saints` appears in `provision_then_every_subcommand_
against_sqlite` solely as `assert!(saints.contains(session))` (`src/cli/mod.rs:531`), which
would pass with the ordering reversed or the Canonical filter dropped. Since T8.3 cannot
dedupe the implementation, the compensating control is an assertion that
`canonical_memories_from_graph(&g)` equals `Memory::canonical_memories()` on one shared graph.
Without it, "if the sort order ever changes, both copies must move together" is a comment, not
a constraint.

---

## Verified holds (attacked, did not break)

1. **Readers never acquire the writer lease — pinned, and the pin bites.** Mutation: a direct
   `store.acquire_lease(...)` inserted into `cli::stats::run`.
   `tests/cli_write_lease.rs` failed immediately with the honest message
   `stats is a reader and must succeed while serve holds; stderr= lambo stats: mutant: lease
   held by agent-a@…`. Property 1's lease half is genuinely guarded.
2. **No reader calls `Memory::build` / `Memory::builder` / `acquire_lease` / `spawn`, and no
   command rebuilds a store or embedder.** A grep for `Memory::build|Memory::builder|
   acquire_lease|Daemon::spawn|\.spawn\(\)|CockroachStore::connect|SqliteStore::connect|
   build_embedder|build_store` across `recall.rs`, `saints.rs`, `stats.rs`, `inspect.rs`,
   `mod.rs` returns hits only in `open_writer` (the writer path) and in test scaffolding.
   Properties 1 and 3 hold structurally.
3. **Writers always release, including on the op-failure path.** `close_writer` is called
   unconditionally by all four write verbs (`derive.rs:99`, `record_action.rs:63`,
   `reserve.rs:68`, `reserve.rs:84`) and there is **no `?` between `open_writer` and
   `close_writer`** in any of them, so no early return can skip the release. `close_writer`
   (`src/cli/mod.rs:78-91`) surfaces a close failure even when the op succeeded, and reports
   both when both fail. `open_writer` failing means no `Memory` exists, so there is nothing to
   close — correct, per probe L.
4. **Lease release after a *successful* write is pinned.** Mutation: `close_writer` returning
   early with `std::mem::forget(mem)` on `Ok`. Caught three ways —
   `cli::sqlite_tests::provision_then_every_subcommand_against_sqlite` and
   `cli::tests::cli_mcp_differential_derive_record_recall` fail, and `tests/cli_write_lease.rs`
   fails because `serve` can no longer acquire the session. (Legibility of that third
   signal is T83-9.)
5. **The CLI↔MCP differential genuinely pins content parity.** Mutation: CLI derive writing
   `format!("{} MUTANT", args.content)`. Exactly
   `cli::tests::cli_mcp_differential_derive_record_recall` failed. The test is not vacuous:
   beyond comparing sorted `hit_contents`, it independently asserts three known needles appear
   in **both** context blocks (`src/cli/mod.rs:359-364`), so an empty-vs-empty pass is
   impossible. Property 6 holds for concept/action content.
6. **The MCP extract is behaviour-preserving.** `git diff 593ff73^..593ff73 -- src/mcp/server.rs`
   is a pure move: `MAX_*`, `check_size`, `clamp_cfg_default`, `Focus`, `FocusCandidate`,
   `resolve_focus`, `render_neighbourhood` deleted and imported from `cli::caps` / `cli::inspect`;
   three `*_impl` methods widened to `pub(crate)`; the local `check_size` kept as a thin
   `validate_size(...).map_err(bad_param)` wrapper so the **error class is unchanged**. No tool
   schema, no `CallToolResult` shape, no `is_error` behaviour changed. The moved bodies are
   line-for-line identical modulo type-path qualification. Property 4 holds.
7. **`src/main.rs` is append-only where it matters.** Every deleted line is one of T8.3's own
   stubs (`recall`/`saints`/`inspect`/`stats`/`provision` `println!` placeholders and their
   flags), the two import lines, or the `Resolved::StoreOnly` variant gaining
   `{ store, kind }`. The `Serve` arm — `init_tracing`, transport parse, `ServeOptions`,
   `runtime.block_on(serve(...))`, the `shutdown_background()` comment block — is untouched, and
   `Demo` is still a stub. Property 10 holds.
8. **One construction site.** `resolve_for_command` (`src/main.rs:252-267`) is the only place
   backends are built; each command receives them. `run_async` always calls
   `runtime.shutdown_background()` (`src/main.rs:289`) on both the success and error paths, so
   probe A's third question is satisfied for every subcommand.
9. **No graph lock is held across an `.await` anywhere in `src/cli/**`.** `stats` holds the
   read guard only across `format!`; `inspect` holds it across `resolve_focus` and
   `render_neighbourhood`, both synchronous and both documented as such
   (`src/cli/inspect.rs:102-104`); `saints` takes a temporary guard inside one call expression;
   `recall` never takes one itself. Property 9 holds.
10. **`provision` really does bootstrap SQLite.** `tests/cli_provision_sqlite.rs` re-run green:
    subprocess `lambo provision` on a fresh file, then an empty-session `recall` (no
    `no such table`), then `derive`, then a `recall` that finds `user schema`. Memory is a
    no-op success with its own test; Cockroach wraps the script; **DSN is not a CLI flag**
    anywhere in `Commands` (confirmed by reading all eleven variants). Property 5 holds.
11. **Caps and control-char behaviour match across surfaces.** Both surfaces share the same
    constants and the same `check_size`, and both apply a non-empty check plus a size check to
    every client string on `derive` / `record_action` (`src/cli/derive.rs:45-74` vs
    `src/mcp/server.rs:609-657`). CLI additionally bounds `session` and `agent`. The one shared
    gap — neither surface caps the *number* of `parent_of` pairs — is pre-existing T8.2
    behaviour, not a T8.3 regression.
12. **The CLI does not echo raw control bytes, even where clap owns the error.** T8.2
    documented an uninterceptable byte-echo residual for `WireConceptType`. I expected the same
    on `--kind` and it is **not** there: `lambo derive --kind $'bad\x01val'` produces
    `error: invalid value 'badval' for '--kind <KIND>'`, and a hexdump confirms no `0x01` byte
    in the output — clap strips it. The CLI is strictly better than MCP here.
13. **`stats` does not lie.** `flush_lag` / `log_depth` / `daemon_cycles` /
    `canonization_cycles` are literal `n/a` with an explicit "writer-only; this is a reader
    process" note (`src/cli/stats.rs:24-37`), never zeros. Asserted by
    `provision_then_every_subcommand_against_sqlite` on both `"n/a"` and `"writer-only"`.
14. **Help is authored for every T8.3 subcommand and flag.** `every_subcommand_and_required_arg_
    has_help` walks recursively and requires `about`/`long_about` per subcommand and
    `help`/`long_help` per arg; `demo` is skipped with an in-code justification naming T8.4
    (`src/main.rs:542-545`). I read every variant: all eleven have doc comments, and every
    T8.3 flag carries an explicit `help`. Property 8 holds. (The walk does not cover
    root-level globals such as `--config`; that arg does have help, so this is a latent gap,
    not a defect — folded into T83-3's fix.)
15. **The shipped flag set is F18-clean.** Independent of the weak guard (T83-3), no
    subcommand accepts a client clock in any form — no timestamp, `created_at`, `now`,
    `when`, `date`, `occurred_at`, or `logical_time` flag exists. `--ttl-seconds` is a
    duration, not an instant.

---

## Test-pinning scorecard (5 mutations, all reverted)

| # | Mutation | File | Caught? | By |
|---|---|---|---|---|
| C | `close()` skipped on the success path (`mem::forget`) | `src/cli/mod.rs` | **YES** | 2 lib tests + `cli_write_lease` (indirectly — T83-9) |
| D | `Daemon::spawn()` added to reader recall | `src/cli/recall.rs` | **NO** | — → **T83-4 (P2)** |
| E | reader `stats` acquires the lease | `src/cli/stats.rs` | **YES** | `cli_write_lease` readers loop, clear message |
| F | `parent_of` ends swapped | `src/cli/derive.rs` | **NO** | — → **T83-1 (P1)** |
| J | CLI derive writes `"<content> MUTANT"` | `src/cli/derive.rs` | **YES** | `cli_mcp_differential_derive_record_recall` only |
| — | `--occurred-at` flag with field `stamp` | `src/main.rs` | **NO** | — → **T83-3 (P2)** |

Three of six survived. All three survivors are missing *guards* on correct code, not shipped
defects — which is precisely the class that regresses silently later.

---

## Rulings on the seven implementor self-flags

1. **CLI `reserve` cannot outlive the command — CONFIRMED honest, ESCALATED in part.** The
   design is right: reserve/release sharing a long-lived writer would break "open, op, close"
   and is not T8.3's to invent, and the sqlite test correctly asserts that a later `release`
   **fails** rather than inventing a lock (`src/cli/mod.rs:585-588`). Not a trap. But the
   success *message* oversells it → **T83-10 (P3)**.
2. **Reader `recall` uses `Config::default()` — CONFIRMED, correctly deferred.** Identical to
   `lambo serve` today; T82-12 already owns "`--config` cannot reach any product knob" and is
   dispositioned to T8.4. Not T8.3's to fix. No new finding.
3. **`canonical_memories` scan copied — CONFIRMED, ESCALATED.** The duplication itself is
   forced (`memory.rs` is out of `owns`), but the compensating differential assertion is
   absent and is cheap → **T83-11 (P3)**.
4. **Live cockroach `saints`/`stats` not executed — CONFIRMED, accurately disclosed, accepted.**
   The test is correctly `#[ignore]`d on `LAMBO_COCKROACH_DSN`, compiled only under
   `store-cockroach`, and panics with an explicit explanation if run `--ignored` without a DSN
   (`src/cli/saints.rs:82-87`). Default `cargo test` needs no cluster. The Handoff Log says so
   plainly. This is the right way to carry an unrunnable gate; no finding.
5. **Provision parses `LamboFile` twice — CONFIRMED, harmless.** Two file/env parses, still
   exactly **one** store construction; returning the kind from `resolve_store_only` would have
   required editing `src/resolve.rs`, outside `owns`. The comment says so
   (`src/main.rs:260-262`). No finding.
6. **Differential compares hit texts, not scores — CONFIRMED and adequate.** Scores legitimately
   differ (MCP recalls through a spawned daemon, the CLI reader does not), and mutation J proves
   the text comparison still bites on content divergence. Comparing scores would pin an
   artifact of the surface, not parity. **Rejected as a defect** — but note it is also what makes
   the differential blind to T83-1.
7. **Extra `--max-tokens` / `--traversal-depth` on `recall` — REJECTED as a defect.** The task's
   load-bearing requirement is CLI↔MCP *parity*; MCP `lambo_recall` exposes both knobs, so
   dropping them to match the yaml's minimal `--session --query --top-k` would **reduce**
   parity. Both are bounded by the shared `MAX_*` constants and both fall back to config
   defaults when omitted. Keep them; the yaml is the looser spec here.

---

## Honest gaps in this review

- **Cockroach was never exercised.** No `LAMBO_COCKROACH_DSN` and no cluster, so the
  `provision` cockroach arm (T83-6), the live `saints`/`stats` test, and cockroach lease
  behaviour under the CLI are all read-only conclusions. T83-6 in particular is asserted from
  code, not demonstrated.
- **`--features store-cockroach,store-memory,fixtures` and `cargo check
  --no-default-features`** are claimed clean in the Handoff Log; I ran only the three gates the
  brief listed plus both test suites. Not disputed, not re-verified.
- **T83-5's recall degradation was not demonstrated end-to-end** — it needs a store claiming
  `VECTOR_SEARCH` with a failing embedder, which the available sqlite/memory stores do not
  provide. The absence of a subscriber and the swallowed `warn!` are both unambiguous on
  reading.
- **HTTP/`serve_web` and `demo` are out of scope** (T8.5 / T8.4) and were not examined beyond
  confirming `main.rs` leaves the demo stub alone.

---

## Disposition — REQUEST CHANGES

**The five central properties I could attack directly all held.** Readers are genuinely
lease-free and genuinely do not build writers; writers open exactly one `Memory` and release it
on every path including failure; there is one construction site; the validators are shared with
no drift and no behaviour change to MCP; `provision` actually bootstraps SQLite; `main.rs` did
not touch the serve lifecycle. This is careful work and the self-flags were substantially
accurate — five of seven survived scrutiny, and the two I escalated were escalated on their
*guards*, not their designs.

What must change is that three of the properties the task is *about* are unpinned, and one
input syntax silently writes wrong data.

**P1 — must fix before T8.3 is done:**

- **T83-1** — assert the `Hierarchical` edge direction after a CLI `derive --parent-of`
  (`edge_between(parent, child, EdgeType::Hierarchical)` on the resulting graph). An inversion
  must fail a test.

**P2 — fix unless it forces new scope:**

- **T83-2** — make `--parent-of` refuse values with more than one colon, with a usage error
  naming the ambiguity, and say so in the flag help. (Do **not** switch to `rsplit_once`.)
- **T83-3** — widen the F18 CLI guard to `arg.get_long()` and `arg.get_all_aliases()` in
  addition to `get_id()`, and match banned tokens as substrings. Mirror T82-4's fix.
- **T83-4** — pin "readers never spawn GC": after a reader `recall`, assert no daemon
  mutation occurred (epoch and canonization statuses unchanged, or no daemon events emitted).

**P3 — fix if cheap, else defer with a named inheritor:**

- **T83-9** (cheap, do it) — add an explicit lease-released assertion after the no-serve derive
  in `tests/cli_write_lease.rs`, so the property is named and does not cost 20 s.
- **T83-10** (cheap, do it) — reword `reserve`'s success text: the reservation ends when this
  process exits, not "on restart"; drop or relabel the `until <timestamp>`.
- **T83-11** (cheap, do it) — one assertion that
  `cli::saints::canonical_memories_from_graph` agrees with `Memory::canonical_memories` on a
  shared graph.
- **T83-8** (cheap, do it) — correct the Handoff Log's `store-sqlite` integration count and use
  one counting convention for both feature sets.
- **T83-6** — require a repo-root marker beside `scripts/provision.sh` and echo the resolved
  path before executing. If deferred, inherits to **T8.7** (ops/provision hardening); if no
  such task exists, **T8.8** (install docs) must at minimum document the cwd-sensitivity.
- **T83-5** — surface the skipped vector leg in `recall`'s output rather than a dropped
  `warn!`. If deferred, inherits to **T8.4** alongside **T82-12**, which already owns CLI
  config/knob plumbing.
- **T83-7** — give the three non-embedding readers a store-only resolution. If deferred,
  inherits to **T8.5** (which touches the `Resolved` enum for `serve_web` anyway).

No finding requires re-litigating T8.1, T8.2 or T8.6. Nothing in `src/mcp/server.rs` needs to
change — the extract is clean.

---

## R1 remediation (2026-08-14) — all 11 FIXED, none deferred

| Id | Disposition | How | Pin |
|---|---|---|---|
| T83-1 | **FIXED** | Direction unchanged; assert Hierarchical parent→child after CLI derive | `cli::tests::parent_of_writes_hierarchical_edge_parent_to_child` |
| T83-2 | **FIXED** | Refuse >1 colon; usage names ambiguity; help updated; not `rsplit_once` | `derive::tests::parent_of_with_more_than_one_colon_is_usage_naming_ambiguity` |
| T83-3 | **FIXED** | F18 walks id + long + aliases; substring match | `tests::f18_no_cli_flag_accepts_a_client_timestamp` |
| T83-4 | **FIXED** | No `.spawn()` in production recall.rs; epoch/status snapshot after reader recall | `cli::tests::reader_recall_does_not_spawn_gc_or_mutate_epoch` |
| T83-5 | **FIXED** | Skipped vector leg / daemon warnings printed as `⚑` lines | `cli::tests::recall_prints_skipped_vector_leg_when_embed_fails` |
| T83-6 | **FIXED** | `lambo` Cargo.toml marker; walk bounded at 16; path echoed | `provision::marker_tests::*` |
| T83-7 | **FIXED** | saints/stats/inspect use `resolve_store_only` via `needs_embedder()` | `tests::saints_stats_inspect_provision_resolve_store_only` |
| T83-8 | **FIXED** | Handoff: 3+1 doctest default, 8+1 sqlite | `dev-diary/PHASE-8-surface.md` T8.3 task-agent entry |
| T83-9 | **FIXED** | Second writer derive after no-serve success names lease-release | `tests/cli_write_lease.rs` |
| T83-10 | **FIXED** | Ends when this process exits; TTL relabelled | sqlite reserve assertions |
| T83-11 | **FIXED** | CLI scan == `Memory::canonical_memories` on one graph | `cli::saints::parity::canonical_memories_from_graph_agrees_with_memory` |
