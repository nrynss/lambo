# #33 D3: before/after recall comparison of a split

`lambo session verify` answers the question the split itself cannot: what
will an agent for each project see after the cut-over, compared with what it
sees today? It replays a fixed per-project query set against the mixed source
session (plain recall) and against the split output (layered recall, the
project's session plus the layer stack the serving `[serve]` table gives it),
and compares the answers by origin identity. D1 classifies, D2 writes the
targets; D3 reads both and writes nothing but its own report.

## Shape

- **One construction site.** `resolve::resolve_verify_io` opens the source
  as classification does (immutable SQLite copy or snapshot) and the split
  output with the same immutable read-only connection. `--config` must be the
  serving `lambo.toml`. `LamboFile::load_resolved` parses all of it and
  applies the environment overlays, `[store]` included; verify then reads
  only the store's kind (it must be SQLite, with no `[recall]` tier) and never
  builds or opens a store from it, so a rig config cannot make verify touch
  the live store. `[embedder]`, `[daemon]`, the promotion policy and
  `[serve]` are used. The product config and the embedding contract come
  from two helpers `resolve_backends` shares (`process_config`,
  `embedding_contract`), so verify runs with the knobs a serve would.
- **The split is rebuilt, not trusted.** The CLI reruns `build_split` on the
  source and plan (a plan that does not seal against the source is refused
  there) and takes digests and placements from that rebuild, never from the
  D2 report file, which is unauthenticated. The output must hold exactly this
  split's targets: `verify_split_destination` (schema, receipt table,
  ownership), then `read_verified_split_target` per target, one read
  transaction each: every target receipted, the receipt equal to the expected
  one, the rows equal to the rebuilt target in canonical order.
- **Library.** `session::verify_split` takes the source snapshot, the plan,
  the rebuilt split, the loaded targets, the serving `[serve]` table, the
  query set, the aggregate-only sessions, the knobs, the embedder and the
  config, and returns a serializable `VerifyReport`. It re-checks the source
  digest against the split and plan and every target digest itself, so a
  library caller cannot hand it inputs that disagree.

## Serving model

- **Pinning.** The pinned sessions and the default are worked out by
  `mcp::serve::pin_sessions` with no `--session`, exactly as an HTTP serve of
  that config pins them (`[serve] sessions`, `default_session`, the
  `max_attached` bound), and the stacks by `ServeConfig::resolve_layers` over
  them. A queried project that is not pinned would be attached on demand,
  and on-demand sessions get no layers in v1, so it is refused rather than
  measured unlayered. A project or layer the split wrote no rows for is
  refused too.
- **SQLite holder.** The replay measures what a SQLite holder does: vectors
  ranked in the session's own graph (`VectorCandidates::for_holder` chooses
  that over an exact-scan store), first-cycle scores and hot list at a
  pinned clock, no access history. Another store kind or a recall tier ranks
  vectors elsewhere, so `resolve_verify_io` refuses those configs, and the
  report's `measures` field and the summary say what was measured.

## Replay

- Each session is materialized as a holder loads it
  (`store::load::materialize`, shared with `load_session_async`: legacy
  vector quarantine, intents out, index rebuilt, graph validated) into an
  **unspawned** daemon. `Daemon::prime_offline` runs a cycle's steps 1 and 2
  once: rescore, published at the graph's epoch so recall is not in cold mode,
  and hot-list detection (`detect_into_hot_list`, extracted from `run_cycle`
  and shared with it). No access ledger, no events, no GC: the graph is never
  written.
- **One pinned clock.** The daemon gets `with_clock(at)`, default the
  source's newest row (`--at` overrides). Recall re-validates hot-list
  entries at the daemon's clock at read time, so a stale session's warning
  follows that clock, not the wall clock.
- **One embedding per query.** Each query is embedded once with the
  configured embedder and the vector goes to both sides (the before
  recall, the after primary and every layer). A failed embed refuses the
  run: one keyword-only side would make the comparison meaningless.
- **Before** is `Memory::recall_detailed`'s pipeline without the access note
  (`Daemon::recall_with`). **After** follows `Memory::recall_layered_slots`
  over live layers: the primary's text pipeline, each layer's `Route::Layer`
  pipeline with the same vector, the same `layered::merge` with the same
  `MergeParams` (`top_k`, `max_tokens`, the config's
  `semantic_match_threshold`, `MAX_VECTOR_DEDUPE_PAIRS`) and
  `format::default_token_count`. Rules inside the merge (the #129 scoped-key
  dedupe among them) apply unchanged.
- Why not real `Memory` holders: they need a lease and a writable store,
  their daemons note accesses and run GC and canonization between queries,
  so one query would change the next and the report would not repeat. The
  cost is a mirrored call order of about forty lines. A parity test pins it:
  on a real D2 SQLite split output, attached alpha and general holders'
  `recall_layered` equals the replay hit for hit (session, id, rank, dedupe
  copies); it fails if the layer floor or the priming is dropped from the
  replay.

## Comparison and disclosure

- An after hit's `(session, id)`, and every dedupe copy in its `also`, maps
  to its origin through the rebuilt placements (target ids, hub copies and
  origin replicas). A hit the placements cannot map refuses the run.
- In scope means placed in the project or one of its layers. recall@k counts
  in-scope before hits ranked at most `k` that are after hits ranked at most
  `k` (forced hits past `k` do not count). Lost, deduped (a before hit now in
  another hit's `also`: not a loss) and gained use every rank.
- **Foreign-primary after hits** replace the earlier "leaked" count, which
  checked a property the mapping already guarantees and so could never be
  non-zero. They are after hits assigned to a session the project does not
  read: in practice a shared hub assigned to another project and copied in.
  Counted by assigned session, per query and per project.
- Removed hits (before hits placed only in sessions the project does not
  read) are counted by assigned session.
- `hub` is the plan's hub set (concepts with `copy_into`), not "placed more
  than once": a hub with one placement is still a hub, and a replicated
  interaction is not.
- **Disclosure.** An origin id is named only when its assigned session is a
  queried project or a layer of one; everything else is counts by assigned
  session. `--aggregate-only <session>` (repeatable) declares a session
  count-only: querying it or layering it is refused, so its ids never appear
  by construction rather than by query choice. Warning lines are reduced to
  kinds (`warning_kind`), never kept raw.
- Per project: the sums, micro and macro recall@k, foreign-primary and
  removed counts by session, after hits by session, hub hits, shift
  statistics over all common hits, annotation and warning kind changes. The
  stdout summary prints these aggregates only.

## Tests

`src/session/verify_tests.rs` (synthetic alpha/beta/general, the D2 fixture
built through D1's reviewed classification and D2's build): byte-identical
reports on rerun and under source row reordering; the pinned clock against
the wall clock; after hits mapping back through hub copies (Cargo.toml and
README.md both ways) and stub replicas; the plan's hub set; another
project's ruling removed and the general ruling answering from the layer for
both projects; foreign-primary counts through the real path (Cargo.toml's
copy in alpha); no layers making general out of scope; dedupe counted apart
from loss; aggregate-only sessions absent from the report JSON (no origin or
target id) and refused as a query or a layer; warning kinds with no text;
recall@k cut at `k`; pinning and layering from `[serve]` (unpinned project,
unpinned layer, unpinned default, a project or layer with zero rows);
refusals for an edited or missing target, an unknown or empty project, a
contract mismatch, `top_k` 0, a failed embed, a source whose digest is not
the split's and unmappable placements; and the SQLite holder parity test.
`src/cli/session.rs` runs split then verify end to end on SQLite: aggregates
only on stdout, both inputs byte-identical and sidecar-free, a 0600 report,
byte-identical reruns, no overwrite, refusals for an edited row, a deleted
receipt, a foreign split hash, another source and a non-split file (none
leaves a report), and refusal of a non-SQLite store and of a `[recall]` tier.

## Follow-ups

- The weight 0.5 and floor 0.40 are uncalibrated until #87; #157 tracks the
  general-layer tuning, and verify is its measuring tool (`[[serve.layer]]`
  knobs in the serving config).
- A recall by image or query vector is not replayed (layered recall is
  text-only, PR 3 decision).

## Review round 1 (adversarial review, 2026-10-13)

1 High, 4 Medium, 7 Low, all fixed after merging main (PR7 #156, whose
scoped-key dedupe lives inside `layered::merge` and so applies here as is).

- H1: the per-query report carried other projects' ids and could carry raw
  warning text. Now: disclosure by assigned session, warning kinds only,
  `--aggregate-only`. The earlier private real-data reports were deleted and
  the run repeated with `--aggregate-only endor`.
- M1: "leaked" could not be non-zero; replaced by foreign-primary after hits
  (a mapping failure now refuses instead).
- M2: stacks came from "every target pinned"; now from `[serve]` as a serve
  pins them, and unpinned (on-demand, unlayered) projects are refused.
- M3: the parity test against `Memory::recall_layered` on SQLite.
- M4: one embedding per query for both sides; an embed failure refuses.
- L1 recall@k cut at `k`; L2 `hub` from the plan; L3 wording for `[store]`
  (parsed, overlays applied, never opened) and for the read-only connection's
  errors (they now name session classify, split and verify inputs); L4 the
  source digest checked against the split and plan; L5 SQLite-holder
  measurement stated and enforced; L6 the runbook below; L7 this note's
  wording (the replay mirrors call order, not "faithful"; "leaks" removed)
  and zero-row coverage.

## Draft cut-over runbook (for the owner)

The owner performs the cut-over. This complements DOGFOOD-SETUP.md section 7
(serving configuration, client registration, worktree checks, rollback) with
the split-specific order. Nothing here was run against the live rig.

0. **Before the cut-over day.** Review the 66 low-confidence items and the
   general-provenance notice on the 2026-10-12 backup, so that at cut-over
   only the delta (items written since) needs labels. Settle #157 (the
   general layer's weight and floor) and put the result in the serving
   config's `[[serve.layer]]`.
1. **Stop the writer.** Pause every client, stop the supervised `lambo serve`
   through the approved flow and confirm the lease is released. From here
   until step 7 nothing may write to `lambo-dev`.
2. **Back up.** Take a fresh completed backup (empty `-wal`), record its hash
   and the last write boundary, and work only on a copy of it.
3. **Classify** with the 0.6.0 release binary, run directly (not through the
   supervised unit), on the copy: the rules, the reviewed labels extended
   with the delta, and the audited corrections.
4. **Split** with the same binary into a new output file. Check the summary
   against the expected accounting (concepts, interactions with origin
   replicas, the five edge categories) and explain every difference. A
   hard-provenance refusal stops the cut-over: fix the plan, do not work
   around it. Rerun once: it must verify with zero rows written.
5. **Verify** with the same binary, with `--config` set to the exact serving
   `lambo.toml` of step 6 (same `[embedder]`, `[daemon]` and `[serve]`, and
   `[store] kind = "sqlite"` set explicitly: without a `[store]` table the
   kind defaults to `memory` and verify refuses, because it measures a SQLite
   holder), the agreed query set and `--aggregate-only endor`. Gate:
   foreign-primary after hits from endor must be 0, and every other
   foreign-primary hit must be an intended shared hub; review every lost hit
   and the changed warning kinds (newly stale project or `general` sessions
   are expected); stop on anything unexplained.
6. **Re-pin and serve a copy.** The re-pin happens only at the 0.6.0
   release, and only after the #36 PR 5 `lambo canon dry-run` review that
   the CHANGELOG requires before re-pinning a writer (no earlier build is
   re-pinned for the split). `chmod 444` the verified output and record its
   hash; copy it to the serving path and point the serve's `[store]` at the
   copy, never at the verified file. Install 0.6.0 (keep the old binary and
   config for rollback) with the section 7.2 `[serve]` table: all eight
   sessions pinned, `default_session = "general-usage"`,
   `layers = ["general"]` and the #157 knobs. Start it and check the startup
   contract and pinning checks.
7. **Rewire clients.** Per section 7.3: a user-scope fallback to `/mcp`
   (general-usage) and a per-project override to `/mcp/s/<session>` (Claude
   Code local scope, Codex trusted project config, Cursor project file), then
   the worktree check (7.4) and the smoke tests (7.5: `lambo_stats` names the
   addressed session, recall shows project and general hits, scoped reads and
   writes stay in their session).
8. **Archive lambo-dev read-only.** Keep the old store file with the
   `lambo-dev` session untouched (`chmod 444` on the file, out of the serve's
   config); read it only through copies. Never delete it, and never run
   `lambo erase-session` on it, during or after the cut-over. Rollback before
   new writes is the old config and binary; after new writes it needs the
   owner's reconciliation decision (section 7.6).
