# `lambo demo` — the spec §13 two-agent scenario (T8.4)

The public getting-started page is [Demo](https://nrynss.github.io/lambo/demo/).
This file is the operator's view: knobs, determinism, and how to check a run.

```bash
lambo demo --scenario rest-api
```

Two agents build one REST API against one session. Agent A lays down
`user schema` / `auth middleware` / `session store` and records the actions that
depend on them; agent B joins on a separate feature; agent A comes back for one
last edit; agent B then asks `recall("update user schema")` and is told, by
memory rather than by a colleague, that the thing it is about to change is
load-bearing and was touched seconds ago.

The scenario lives in `src/cli/demo.rs`. That module's doc comment is the
authority on how it works.

---

## What is real

Everything. There is no code path in `src/cli/demo.rs` that writes a
`CanonizationStatus` or a `canonization_events` row. The three `user schema`
transitions are committed by the real `CanonizationTask` against the real store
predicates (`interaction_span`, `blast_radius`), through the same
`Graph::apply_canonization_transition` write gate that rejects fabricated
transitions. The context block is the T5.3 renderer's output, verbatim.

The demo asserts its own claims before it waits on anything: 12 interactions,
27 concepts, `user schema` blast radius exactly 9. A run that would have
produced a different graph fails with a diagnosis instead of quietly showing a
different demo.

---

## The knobs

Two `Config`s are used. **No threshold is weakened by either** — only
intervals, and one age floor, are compressed, because the demo has to fit in a
three-minute video rather than an afternoon.

| Knob | Spec default | Build phase (acts I–III) | Canonization phase |
|---|---|---|---|
| `canonization_edge_min_age` | 60s | 60s (unused: no cycles run) | **10ms** |
| `canonization_eval_interval` | 60s | **1h** (frozen) | **25ms** |
| `daemon_tick_interval` | 1s | **5ms** | **5ms** |
| `gc_interval` | 10 000 mutations | 10 000 (no sweep runs) | **1 mutation** |
| `backend_flush_interval` | 1s | **5ms** | **5ms** |
| `match_strategy` | Hybrid | **Canonical** | **Canonical** |

`canonization_edge_min_age` is the knob T8.4 names. It is the age floor Stage 2
applies to inbound structural edges and Stage 3 applies to the blast-radius
query — the guard that stops a burst of same-tick edges from inflating either
measure. Compressing it from 60s to 10ms keeps the guard **live** (an edge
written in this cycle still does not count; the engine genuinely waits for it to
age) while letting a session that is seconds old in demo time behave like one
that is an hour old in spec time.

Freezing `canonization_eval_interval` during the build is not cosmetic: it
guarantees no cycle ever evaluates a half-built graph, so the state machine
starts from one deterministic state. `gc_interval` stays at its spec default
over the same span, so no GC sweep can evict a concept out of a partially
written session.

`match_strategy` is pinned to `Canonical` so the write path resolves concepts by
canonical key alone — no embedding lookups on the write path, and therefore no
dependence on an embedder's weights, on the network, or on which backend is
configured. Recall still runs its vector leg when the store claims
`VECTOR_SEARCH`.

**Left at spec defaults, deliberately**, because they are the thresholds the
demo is claiming to satisfy: `canonization_min_peer_count` (20),
`canonization_eval_batch_size` (50), `canonization_repromotion_cooldown` (300s),
`max_canonical_nodes` (1000), `conflict_recency_window` (30s), the scoring and
recall weights, and every stage constant in `src/canon` (`gc_survived >= 3`,
strictly above P90, `distinct >= 3`, `coverage >= 0.3`, `blast_radius > 5`).

### Why GC cannot simply be turned off

The obvious way to keep a demo stable is to stop GC from running. It is not
available here: canonization Stage 1 gates on `gc_survived >= 3`, and GC's
survivor bump is the **only** thing in the system that raises that counter. A
demo with GC disabled has no transitions at all.

So GC runs, and the script is instead a *healthy* session — one where GC's
sub-threshold clause has nothing to collect. `cli::demo::gc_headroom` measures
every concept's distance from the eviction bar and the demo refuses to run below
`MIN_GC_HEADROOM` (1.25×), naming the concept. The run prints the margin:

```
GC headroom: closest to the eviction bar is 'users table migration' at 1.55×
```

---

## Determinism

"Works three times in five" is not done. The scenario is built so its outcome is
a **fixed point**, not a snapshot taken at a lucky instant:

1. **The script is fixed** — the same twelve interactions, same order, same
   contents, every run.
2. **No wall-clock wait decides anything.** Every wait is a bounded poll on an
   observable condition: a status in the graph, an audit-trail length, a
   `gc_survived` floor, a daemon event, a completed canonization cycle.
3. **The state machine is driven to a unique fixed point.** After the last write
   the demo *settles* (`gc_survived` over Stage 1's floor for every concept, one
   awaited GC sweep at a time) and then *quiesces* (three consecutive
   canonization cycles with no new audit rows). With the graph frozen and every
   scoring dimension session-relative, the score table is a pure function of the
   graph, and `user schema` is the only concept that can leave the non-Canonical
   peer set — so the admitted set is the same regardless of the order cycles ran
   in.
4. **Sibling concepts derived in one interaction carry distinct concept types**,
   so structurally identical siblings never score exactly equal. An exact tie is
   broken by `NodeId`, and node ids are random UUIDs.
5. **The script is paced** (`STEP_PACING`, 10ms between interactions). The
   `recency` dimension is each concept's position inside the session's real
   temporal extent; twelve writes issued back to back land microseconds apart,
   so their interior spacing would be scheduler jitter. Pacing makes the extent
   a property of the script. It also makes the narration readable on screen.

### What is normalized, and why

`DemoOutcome` is the ×2 comparison. Three things are masked in it, each because
it is a genuine measurement rather than a property of the script:

| Masked | Rendered as | Why |
|---|---|---|
| the conflict line's age | `<n>` | the true age of agent A's write at read time |
| the composite score | `<s>` | its `recency` term is a wall-clock measurement |
| node ids | `<node>` | `Uuid::new_v4()` |

Everything else is compared byte for byte, including the hit **ordering**, every
concept's content, the `[Entity, canonical]` marker, `blast radius 9`, the ⚑
line and the conflict sentence. The demo prints the real values; only the
comparison sees the placeholders.

Spec §13 narrates "eleven seconds ago". That is the age at the instant the
video's agent B asks. On a laptop the whole session replays in about a second,
so the real line reads `Agent A wrote to it 0 seconds ago`. It is not padded to
match the prose.

---

## Sessions are fresh (P6 review R3-1)

On SQLite and CockroachDB, canonization state is **not** restored over an
existing session, so re-running the scenario into a used session silently
produces a demo that does not transition. `lambo demo` therefore mints a fresh
session id per run:

```
session      demo-rest-api-172e62ae-3f34-4b6b-af6a-7b29d10b442d   (fresh per run — P6 R3-1)
```

`--session <id>` exists so a reader CLI can be pointed at a known id, and is
documented as fresh-only. Do not re-run into one.

---

## Tests

| Test | Command |
|---|---|
| ×2 identical on `MemoryStore` | `cargo test --test t84_demo` |
| ×2 identical on SQLite | `cargo test --features store-sqlite --test t84_demo` |
| script + normalizer unit tests | `cargo test cli::demo` |

The ×2 tests run the whole scenario twice in one process, against a store that
already holds the first run's session, and assert the two `DemoOutcome`s are
equal.
