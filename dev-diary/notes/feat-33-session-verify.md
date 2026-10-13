# #33 D3: before/after recall comparison of a split

`lambo session verify` answers the question the split itself cannot: what
will an agent for each project see after the cut-over, compared with what it
sees today? It replays a fixed per-project query set against the mixed source
session (plain recall) and against the split output (layered recall, the
project's session plus its `[serve] layers` stack), and compares the answers
by origin identity. D1 classifies, D2 writes the targets; D3 reads both and
writes nothing but its own report.

## Shape

- **One construction site.** `resolve::resolve_verify_io` opens the source
  as classification does (immutable SQLite copy or snapshot) and the split
  output with the same immutable read-only connection. From `--config` it
  reads `[embedder]`, `[daemon]`, the promotion policy and `[serve]`; it never
  reads or builds `[store]` or `[recall]`, so a rig `lambo.toml` cannot make
  verify open the live store. The product config and the embedding contract
  come from two helpers `resolve_backends` now shares (`process_config`,
  `embedding_contract`), so verify runs with the knobs a serve would.
- **The split is rebuilt, not trusted.** The CLI reruns `build_split` on the
  source and plan (a plan that does not seal against the source is refused
  there) and takes digests and placements from that rebuild, never from the
  D2 report file, which is unauthenticated. The output must hold exactly this
  split's targets: `verify_split_destination` (schema, receipt table,
  ownership), then `read_verified_split_target` per target, one read
  transaction each: every target receipted, the receipt equal to the expected
  one, the rows equal to the rebuilt target in canonical order.
- **Pure-ish library.** `session::verify_split` takes the source snapshot,
  the rebuilt split, the loaded targets, the resolved layer stacks, the query
  set, the knobs, the embedder and the config, and returns a serializable
  `VerifyReport`. It re-checks target digests itself, so a library caller
  cannot hand it targets that do not match.

## Replay

- Each session is materialized as a holder loads it
  (`store::load::materialize`, now shared with `load_session_async`: legacy
  vector quarantine, intents out, index rebuilt, graph validated) into an
  **unspawned** daemon. `Daemon::prime_offline` runs a cycle's steps 1 and 2
  once: rescore, published at the graph's epoch so recall is not in cold mode,
  and hot-list detection (`detect_into_hot_list`, extracted from `run_cycle`
  and shared with it). No access ledger, no events, no GC: the graph is never
  written.
- **One pinned clock.** The daemon gets `with_clock(at)`, default the
  source's newest row (`--at` overrides). Recall re-validates hot-list
  entries at the daemon's clock at read time, so a stale session's
  "Session inactive N seconds" line follows the clock. The first private run
  used the wall clock for that re-validation and two runs differed by those
  seconds; the fix pins it, and a regression test runs the same verify twice
  a second apart.
- **Before** is `Memory::recall_detailed` without the access note: embed
  once, `Daemon::recall_with`, the embed warning leading. **After** mirrors
  `Memory::recall_layered_slots` over live layers: embed once in the primary,
  the primary's text pipeline, each layer's `Route::Layer` pipeline with the
  primary's vector, the same `layered::merge` with the same `MergeParams`
  (`top_k`, `max_tokens`, the config's `semantic_match_threshold`,
  `MAX_VECTOR_DEDUPE_PAIRS`) and `format::default_token_count`. The vector
  leg ranks in each session's own graph, which is what a SQLite holder uses
  (`VectorCandidates::for_holder`).
- Layer stacks come from `ServeConfig::resolve_layers` with every target
  pinned, so `[serve.session_layers]` overrides and `[[serve.layer]]` knobs
  apply exactly as they would at a serve's start.
- Why not real `Memory` holders: they need a lease and a writable store,
  their daemons note accesses and run GC and canonization between queries,
  so one query would change the next and the report would not repeat. The
  cost is a mirrored orchestration of about forty lines. It calls the same
  daemon entry points and the same merge, so the ranking and merge rules are
  not duplicated, only the order of calls; a change to
  `Memory::recall_layered_slots` must be mirrored in `layered_recall`.

## Comparison

- An after hit's `(session, id)` maps to its origin through the rebuilt
  placements (every target id, hub copies and origin replicas included); an
  unmapped hit refuses the run. Dedupe copies in a hit's `also` are mapped
  too.
- In scope means placed in the project or one of its layers. Per query:
  overlap and recall@k over in-scope before hits; lost; **deduped** (a before
  hit that is now another hit's `also`, found in the synthetic fixture whose
  2-d vectors are nearly collinear: not a loss, so it is counted apart);
  gained; rank shifts with annotation kinds gained and lost; removed foreign
  hits (placed only in sessions the project does not read), with the session
  the split assigned them to; leaks (an after hit whose origin is not placed
  in its session; none by construction, kept as a check); warning lines and
  response annotation kinds that changed.
- Per project: the sums, micro and macro recall@k, after hits by session,
  shared-hub hits, shift statistics, annotation changes by kind, and queries
  with changed warnings. The stdout summary prints these aggregates only.

## Tests

`src/session/verify_tests.rs` (synthetic alpha/beta/general, the D2 fixture
built through D1's reviewed classification and D2's build): byte-identical
reports on rerun and under source row reordering; the pinned clock against
the wall clock; after hits mapping back through hub copies (Cargo.toml and
README.md both ways) and stub replicas; another project's ruling removed and
the general ruling answering from the layer; no layers making general out of
scope; a constructed leak; dedupe counted apart from loss; refusals for an
edited or missing target, an unknown project, an empty project, a contract
mismatch and `top_k` 0. `src/cli/session.rs` runs split then verify end to
end on SQLite: aggregates only on stdout, both inputs byte-identical and
sidecar-free, a 0600 report, byte-identical reruns, no overwrite, and
refusals for an edited row, a deleted receipt, a foreign split hash, another
source and a non-split file, none of which leaves a report.

## Follow-ups

- The weight 0.5 and floor 0.40 are uncalibrated until #87; verify is the
  measuring tool for that (`[[serve.layer]]` knobs in the verify config).
- A recall by image or query vector is not replayed (layered recall is
  text-only, PR 3 decision).

## Draft cut-over runbook (for the owner)

The owner performs the cut-over. This complements DOGFOOD-SETUP.md section 7
(serving configuration, client registration, worktree checks, rollback) with
the split-specific order. Nothing here was run against the live rig.

1. **Stop the writer.** Pause every client, stop the supervised `lambo serve`
   through the approved flow and confirm the lease is released. From here
   until step 7 nothing may write to `lambo-dev`.
2. **Back up.** Take a fresh completed backup (empty `-wal`), record its hash
   and the last write boundary, and work only on a copy of it.
3. **Classify.** `lambo session classify` on the copy with the rules, the
   reviewed labels and the audited corrections. Reviewed labels must cover
   the source exactly, so items written after the labelled backup need labels
   (or a reviewed automatic pass) first; review the low-confidence items and
   the general-provenance notice before going on.
4. **Split.** `lambo session split` into a new output file. Check the summary
   against the expected accounting (concepts, interactions with origin
   replicas, the five edge categories) and explain every difference. A
   hard-provenance refusal stops the cut-over: fix the plan, do not work
   around it. Rerun once: it must verify with zero rows written.
5. **Verify.** `lambo session verify` with the serving `[serve]` table and
   the agreed per-project query set. Leaks must be 0. Review every lost hit
   and the warnings that changed (a project or `general` that is newly stale
   is expected); stop on anything unexplained.
6. **Re-pin.** Install the release that carries layered serving, the split
   and verify (0.6.0), keeping the old binary and config for rollback.
   Point the serve's `[store]` at the verified split output (or a copy of
   it), with the section 7.2 `[serve]` table: all eight sessions pinned,
   `default_session = "general-usage"`, `layers = ["general"]`. Start it and
   check the startup contract and pinning checks.
7. **Rewire clients.** Per section 7.3: a user-scope fallback to `/mcp`
   (general-usage) and a per-project override to `/mcp/s/<session>` (Claude
   Code local scope, Codex trusted project config, Cursor project file), then the worktree check (7.4) and the smoke tests (7.5:
   `lambo_stats` names the addressed session, recall shows project and
   general hits, scoped reads and writes stay in their session).
8. **Archive lambo-dev read-only.** Keep the old store file with the
   `lambo-dev` session untouched (`chmod 444` on the file, out of the
   serve's config); read it only through copies. Never delete it, and never
   run `lambo erase-session` on it, during or after the cut-over. Rollback
   before new writes is the old config and binary; after new writes it needs
   the owner's reconciliation decision (section 7.6).
